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
//! [--bitrate KBPS] [--codec auto|h264|hevc|av1] [--input-test|--mic-test|--touch-test]
//! [--pin HEX|--pair -] [--compositor NAME] [--gamepad NAME] [--trajectory FILE] |
//! --discover [SECS]`
#![forbid(unsafe_code)]

mod trajectory;

use anyhow::{anyhow, Context, Result};
use punktfunk_core::config::GamepadPref;
use punktfunk_core::config::Role;
use punktfunk_core::input::{InputEvent, InputKind};
use punktfunk_core::packet::FLAG_PROBE;
use punktfunk_core::quic::{
    endpoint, io, window_loss_ppm, BitrateChanged, CursorRenderMode, DeliveryReport, Hello,
    LossReport, ProbeRequest, ProbeResult, Reconfigure, Reconfigured, RequestKeyframe, SetBitrate,
    Start, Welcome,
};
use punktfunk_core::transport::UdpTransport;
use punktfunk_core::{CompositorPref, Mode, PunktfunkError, Session};
use std::io::Write;

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
    /// `--pair -` — run the pairing ceremony instead of a session.
    pair: Option<String>,
    /// `--name LABEL` — how the host labels this client when pairing.
    name: String,
    /// `--compositor NAME` — request a host compositor backend (auto|kwin|wlroots|mutter|gamescope|hyprland).
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
    /// `--discover [SECS]` — browse the LAN for native (`_punktfunk._udp`) hosts for `SECS`
    /// seconds (default 4), print what's found, and exit. No connection is made.
    discover: Option<u64>,
    /// `--trajectory FILE` — stream on the shared client pump (the one the desktop and TV
    /// clients run) in Automatic and write one JSON line per ABR report window, plus a
    /// summary carrying the link simulator's metrics. The netem rig's client half.
    trajectory: Option<String>,
    /// `--link <achievable_kbps>[:<capacity_kbps>]` — what the shaped path is, for the two
    /// summary metrics a client cannot measure from inside its own session.
    link: trajectory::Link,
    /// `--profile NAME` — the row name the trajectory summary prints under.
    profile: String,
    /// `--decoder-hold` — hold the picture after a lost frame until one that re-anchors it
    /// arrives, asking for a keyframe while held, as the shipped TV client does. Without it
    /// the rig resumes as soon as frame indexes line up, which no decoder can do.
    decoder_hold: bool,
    /// `--clock-resync` — after the connect-time skew handshake, immediately run a SECOND
    /// handshake on the same control stream and assert both estimates are sane and consistent:
    /// the headless validator for the host answering `ClockProbe` at any time (what the native
    /// clients' mid-stream re-sync relies on). Aborts the session when the re-probe fails.
    clock_resync: bool,
}

fn parse_mode(m: &str) -> Option<Mode> {
    let mut it = m.split('x');
    Some(Mode {
        width: it.next()?.parse().ok()?,
        height: it.next()?.parse().ok()?,
        refresh_hz: it.next()?.parse().ok()?,
    })
}

fn parse_hex32(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}

fn hex(fp: &[u8; 32]) -> String {
    fp.iter().map(|b| format!("{b:02x}")).collect()
}

/// This client's persistent identity (`~/.config/punktfunk/client-{cert,key}.pem`),
/// generated on first use — presented on every connect so hosts can recognize it once
/// paired.
fn load_or_create_identity() -> Result<(String, String)> {
    let home = std::env::var("HOME").context("HOME unset")?;
    let dir = std::path::PathBuf::from(home).join(".config/punktfunk");
    let (cp, kp) = (dir.join("client-cert.pem"), dir.join("client-key.pem"));
    if let (Ok(c), Ok(k)) = (std::fs::read_to_string(&cp), std::fs::read_to_string(&kp)) {
        // Re-lock a store an older build left world-readable (this key is shared with the other
        // clients' `~/.config/punktfunk/client-key.pem`); best-effort.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
            let _ = std::fs::set_permissions(&kp, std::fs::Permissions::from_mode(0o600));
        }
        return Ok((c, k));
    }
    let (c, k) = endpoint::generate_identity().map_err(|e| anyhow!("generate identity: {e}"))?;
    std::fs::create_dir_all(&dir)?;
    // The certificate is public; the key is the mTLS credential a paired host authorizes for full
    // remote control, so it must not be world-readable — create it 0600 (a plain `fs::write`
    // honors the umask → typically 0644).
    std::fs::write(&cp, &c)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&kp)?;
        f.write_all(k.as_bytes())?;
    }
    #[cfg(not(unix))]
    std::fs::write(&kp, &k)?;
    tracing::info!(cert = %cp.display(), "generated client identity");
    Ok((c, k))
}

fn parse_args() -> Args {
    let argv: Vec<String> = std::env::args().collect();
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
                eprintln!("--compositor takes auto, kwin, mutter, hyprland, wlroots or gamescope");
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
        quit: argv.iter().any(|a| a == "--quit"),
        seconds: get("--seconds").and_then(|s| s.parse().ok()),
        pin,
        remode,
        rebitrate,
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
        decoder_hold: argv.iter().any(|a| a == "--decoder-hold"),
        clock_resync: argv.iter().any(|a| a == "--clock-resync"),
        cursor_capture: argv.iter().any(|a| a == "--cursor-capture"),
        cursor_nochannel: argv.iter().any(|a| a == "--cursor-nochannel"),
    }
}

fn now_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
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
/// `fp` (host cert fingerprint to pin), `pair` (required|optional), `id` (stable host id).
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
                    "  {name:<24} {addr}:{:<6} pair={:<9} fp={fp_short}…",
                    info.get_port(),
                    val("pair"),
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

async fn session(args: Args) -> Result<()> {
    let remote: std::net::SocketAddr = args.connect.parse().context("--connect host:port")?;
    let identity = load_or_create_identity()?;
    let (ep, observed) = endpoint::client_pinned_with_identity(
        args.pin,
        Some((identity.0.as_str(), identity.1.as_str())),
    );
    let ep = ep.map_err(|e| anyhow!("QUIC client endpoint: {e}"))?;
    let conn = ep
        .connect(remote, "punktfunk")
        .context("connect")?
        .await
        .context("QUIC handshake (a pin mismatch fails here)")?;
    match (args.pin, *observed.lock().unwrap()) {
        (Some(_), _) => tracing::info!(%remote, "punktfunk/1 connected — host fingerprint pinned"),
        (None, Some(fp)) => tracing::info!(
            %remote,
            fingerprint = %hex(&fp),
            "punktfunk/1 connected (trust-on-first-use) — pass --pin to verify this host"
        ),
        (None, None) => tracing::info!(%remote, "punktfunk/1 connected"),
    }
    let (mut send, recv) = conn.open_bi().await.context("open control stream")?;
    // Frame every read on the control stream through the resumable reader, exactly as the client
    // pump does: `clock_sync` bounds each read with a timeout, and a frame straddling two wakeups
    // would otherwise leave the stream permanently misaligned for the rest of the run.
    let mut recv = io::MsgReader::new(recv);

    io::write_msg(
        &mut send,
        &Hello {
            abi_version: punktfunk_core::WIRE_VERSION,
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
                let mut c = if args.cursor_capture {
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
                c
            },
            // Like STREAMED_AU above: the shared-core reassembler pins geometry per-frame, so
            // the probe accepts a mid-session shard change (and jumbo growth) up to the
            // receive ceiling — and it's exactly the tool to measure both.
            max_shard_payload: punktfunk_core::config::max_shard_payload() as u16,
            // `0`/`0` is UNSPECIFIED. For the shipping clients that distinction decides the
            // capability bit — `advertised_client_caps` sets it when either field is non-zero,
            // because it keys on "the caller specified a format", so that 48 kHz/16-bit (the
            // cheapest lossless rung) is not the one format nobody can request. This `Hello` is
            // hand-built and does not use that helper, so here the two are set together above and
            // the sentinel is about honesty rather than mechanism: an inert format on the wire
            // invites exactly the misreading that a rate alone asks for something.
            audio_rate_hz: args.audio_format.map(|(r, _)| r).unwrap_or(0),
            audio_bits: args.audio_format.map(|(_, b)| b).unwrap_or(0),
        }
        .encode(),
    )
    .await?;
    let welcome =
        Welcome::decode(&recv.read_msg().await?).map_err(|e| anyhow!("Welcome decode: {e:?}"))?;
    tracing::info!(
        mode = ?welcome.mode,
        fec = ?welcome.fec,
        encrypt = welcome.encrypt,
        frames = welcome.frames,
        compositor = welcome.compositor.as_str(),
        gamepad = welcome.gamepad.as_str(),
        bit_depth = welcome.bit_depth,
        color = ?welcome.color,
        hdr = welcome.color.is_hdr(),
        chroma_444 = welcome.chroma_format == punktfunk_core::quic::CHROMA_IDC_444,
        chroma_format_idc = welcome.chroma_format,
        codec = codec_ext(welcome.codec),
        cipher = if welcome.cipher == punktfunk_core::quic::CIPHER_CHACHA20_POLY1305 {
            "chacha20-poly1305"
        } else {
            "aes-128-gcm"
        },
        "session offer"
    );

    // Reserve our data-plane port, then tell the host to start.
    let probe = std::net::UdpSocket::bind("0.0.0.0:0")?;
    let udp_port = probe.local_addr()?.port();
    drop(probe);
    io::write_msg(
        &mut send,
        &Start {
            client_udp_port: udp_port,
        }
        .encode(),
    )
    .await?;

    // Wall-clock skew handshake on the still-private control stream (before --remode/--speed-test
    // take it): align our clock to the host's so the per-frame capture→received latency is valid
    // across machines. `None` ⇒ an old host that doesn't answer — fall back to a shared clock (0).
    let first_skew = punktfunk_core::quic::clock_sync(&mut send, &mut recv).await;
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
    if args.clock_resync {
        let first = first_skew.as_ref().ok_or_else(|| {
            anyhow!("clock-resync: host never answered the connect-time handshake")
        })?;
        let second = punktfunk_core::quic::clock_sync(&mut send, &mut recv)
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

    // Packet-level receive counters mirrored from `session.stats()` by the data-plane loop. The
    // speed test reads their delta over the burst window so throughput/loss reflect every delivered
    // wire packet (graceful past the FEC budget), not just fully-reassembled probe AUs.
    let rx_wire_packets = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let rx_wire_bytes = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    // Adaptive-FEC loss feedback: the data loop publishes a windowed loss estimate here; in normal
    // stream mode (no speed test / remode) a control-stream task relays it to the host as a
    // LossReport so it can size FEC to the link. u32::MAX = "no fresh sample this window".
    let loss_ppm = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(u32::MAX));
    // Decode-recovery feedback, mirroring the real clients: the data loop publishes the session's
    // cumulative unrecoverable-frame count; the control task requests a keyframe when it grows
    // (the correct loss trigger under infinite GOP — see NativeClient::frames_dropped). Lets the
    // probe exercise the host's IDR-vs-intra-refresh recovery path under injected loss.
    let dropped_frames = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));

    // Mid-stream renegotiation test: after a delay, ask the host to switch modes on the
    // still-open control stream. The stream then carries new-mode AUs (IDR + in-band
    // parameter sets) — ffprobe the --out file to see both resolutions. Mutually exclusive with
    // --speed-test (both own the control stream).
    if let Some((new_mode, after_secs)) = args.remode {
        let mut rs = send;
        let mut rr = recv;
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(after_secs as u64)).await;
            tracing::info!(?new_mode, "requesting mid-stream mode switch");
            if io::write_msg(&mut rs, &Reconfigure { mode: new_mode }.encode())
                .await
                .is_err()
            {
                tracing::error!("Reconfigure write failed");
                return;
            }
            match rr.read_msg().await.map(|b| Reconfigured::decode(&b)) {
                Ok(Ok(ack)) if ack.accepted => {
                    tracing::info!(mode = ?ack.mode, "mode switch ACCEPTED")
                }
                Ok(Ok(ack)) => tracing::warn!(active = ?ack.mode, "mode switch REJECTED"),
                other => tracing::error!(?other, "bad Reconfigured"),
            }
        });
    } else if let Some((new_kbps, after_secs)) = args.rebitrate {
        // Mid-stream adaptive-bitrate test: after a delay, send the SetBitrate the Automatic
        // controller would and await the host's BitrateChanged ack. Host-side this exercises the
        // in-place `reconfigure_bitrate` (no IDR) or the rebuild fallback — the host log says
        // which. The cursor wiggle keeps a damage-driven idle desktop publishing frames through
        // the whole window: the encode loop only drains bitrate requests between frames, and the
        // post-switch AUs are what prove the stream carried on.
        let mut rs = send;
        let mut rr = recv;
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
            let end =
                std::time::Instant::now() + std::time::Duration::from_secs(after_secs as u64 + 6);
            let switch_at =
                std::time::Instant::now() + std::time::Duration::from_secs(after_secs as u64);
            let mut sent = false;
            let mut i = 0u32;
            while std::time::Instant::now() < end {
                let _ = conn2.send_datagram(wiggle(i).encode().to_vec().into());
                i += 1;
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                if !sent && std::time::Instant::now() >= switch_at {
                    sent = true;
                    tracing::info!(new_kbps, "requesting mid-stream bitrate change");
                    if io::write_msg(
                        &mut rs,
                        &SetBitrate {
                            bitrate_kbps: new_kbps,
                        }
                        .encode(),
                    )
                    .await
                    .is_err()
                    {
                        tracing::error!("SetBitrate write failed");
                        return;
                    }
                    match rr.read_msg().await.map(|b| BitrateChanged::decode(&b)) {
                        Ok(Ok(ack)) => tracing::info!(
                            applied_kbps = ack.bitrate_kbps,
                            "BITRATE CHANGE acked by host"
                        ),
                        other => tracing::error!(?other, "bad BitrateChanged"),
                    }
                }
            }
        });
    } else if let Some((target_kbps, duration_ms)) = args.speed_test {
        // Bandwidth probe: after the stream warms up, ask the host to burst FLAG_PROBE filler; measure
        // delivered WIRE packets (session-stat delta) vs. what the host reports putting on the wire.
        let mut ss = send;
        let mut sr = recv;
        let (rxp, rxb) = (rx_wire_packets.clone(), rx_wire_bytes.clone());
        // Per-packet wire size to express delivered bytes as link bytes (header + shard + crypto);
        // every shard is zero-padded to shard_payload so all data packets are this exact size.
        let crypto_overhead = if welcome.encrypt {
            punktfunk_core::packet::CRYPTO_OVERHEAD as u64
        } else {
            0
        };
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
                let _ = conn2.send_datagram(mv.encode().to_vec().into());
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            // Baseline the packet-level counters right before the burst (video is paused during it,
            // so the delta is pure probe traffic plus a sliver of resumed video in the settle).
            let base_pkts = rxp.load(Relaxed);
            let base_bytes = rxb.load(Relaxed);
            tracing::info!(target_kbps, duration_ms, "requesting speed-test probe");
            if io::write_msg(
                &mut ss,
                &ProbeRequest {
                    target_kbps,
                    duration_ms,
                }
                .encode(),
            )
            .await
            .is_err()
            {
                tracing::error!("ProbeRequest write failed");
                return;
            }
            let res = match sr.read_msg().await.map(|b| ProbeResult::decode(&b)) {
                Ok(Ok(r)) => r,
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
            let recv_packets = rxp.load(Relaxed).saturating_sub(base_pkts);
            // bytes_received counts plaintext (header + shard); add per-packet crypto back for the
            // true on-wire byte count.
            let recv_wire_bytes =
                rxb.load(Relaxed).saturating_sub(base_bytes) + recv_packets * crypto_overhead;
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
    } else if args.cursor_capture || args.cursor_nochannel {
        // Capture-model cursor repro. `--cursor-capture`: flip the negotiated cursor channel to
        // "host composites" and drive RELATIVE pointer motion, exactly like a pointer-lock
        // client. `--cursor-nochannel`: the same motion with NO channel at all (a
        // capture-latched client) — the host must composite unprompted. Owns BOTH control
        // halves: the host may send CursorShape (0x50) messages while a negotiated channel is
        // still in its initial client-draws state, and dropping our recv half would fail those
        // writes host-side.
        let flip_channel = args.cursor_capture;
        let mut cs = send;
        let mut cr = recv;
        tokio::spawn(async move {
            if flip_channel {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                match io::write_msg(
                    &mut cs,
                    &CursorRenderMode {
                        client_draws: false,
                    }
                    .encode(),
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
            // Drain (and just log) whatever the host still sends on the control stream.
            while let Ok(b) = cr.read_msg().await {
                tracing::debug!(
                    len = b.len(),
                    ty = b.get(4).copied().unwrap_or(0),
                    "cursor-capture: control message drained"
                );
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
                let _ = wiggle_conn.send_datagram(e.encode().to_vec().into());
                t += 0.2;
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        });
    } else {
        // Normal stream mode: relay the data loop's windowed loss estimate to the host as periodic
        // LossReports, so it can size FEC to the link (adaptive FEC), and — like the real clients —
        // request a keyframe whenever the unrecoverable-frame count grows (100 ms poll = a natural
        // throttle; several drops in a burst coalesce into one request). The control stream is
        // otherwise idle here (remode/speed-test own it in their modes).
        let mut ls = send;
        let lp = loss_ppm.clone();
        let df = dropped_frames.clone();
        // Delivery truth for the host's dead-data-plane check: report what actually landed on the
        // wire, so the probe reproduces a real client's answer rather than the "cannot answer"
        // sentinel — which is exactly what makes it usable for testing that path.
        let rxp = rx_wire_packets.clone();
        tokio::spawn(async move {
            use std::sync::atomic::Ordering::Relaxed;
            let mut last_report = std::time::Instant::now();
            let mut last_dropped = 0u64;
            // Mirrors the real clients' rule (see `pump/data.rs`): report the delivery count every
            // window while it is zero, once when the first packets land, then stop — so a host that
            // predates the message is not flooded with "unknown control message" on a good session.
            let mut delivery_confirmed = false;
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                let d = df.load(Relaxed);
                if d > last_dropped {
                    last_dropped = d;
                    if io::write_msg(&mut ls, &RequestKeyframe.encode())
                        .await
                        .is_err()
                    {
                        break; // control stream gone
                    }
                    tracing::debug!(dropped = d, "unrecoverable frame — requested keyframe");
                }
                if last_report.elapsed() >= std::time::Duration::from_millis(750) {
                    last_report = std::time::Instant::now();
                    let v = lp.swap(u32::MAX, Relaxed);
                    // Independent of whether there is a fresh loss sample: "no fresh sample" is
                    // exactly the shape a dead data plane has, so gating it on one would silence
                    // it in the state it exists to report.
                    let received = rxp.load(Relaxed);
                    if received == 0 || !delivery_confirmed {
                        delivery_confirmed = received > 0;
                        if io::write_msg(
                            &mut ls,
                            &DeliveryReport {
                                packets_received: received,
                            }
                            .encode(),
                        )
                        .await
                        .is_err()
                        {
                            break; // control stream gone
                        }
                    }
                    if v != u32::MAX
                        && io::write_msg(&mut ls, &LossReport { loss_ppm: v }.encode())
                            .await
                            .is_err()
                    {
                        break; // control stream gone
                    }
                }
            }
        });
    }

    // Input plane: scripted events as QUIC datagrams (mouse square + 'A' taps), proving the
    // low-latency input path without a real input device.
    if args.input_test {
        let conn2 = conn.clone();
        let (mw, mh) = (args.mode.width, args.mode.height);
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
                let _ = conn2.send_datagram(mv.encode().to_vec().into());
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
                let _ = conn2.send_datagram(abs.encode().to_vec().into());
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
                        let _ = conn2.send_datagram(key.encode().to_vec().into());
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
                        let _ = conn2.send_datagram(ev.encode().to_vec().into());
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(40)).await;
            }
            tracing::info!("input-test: done");
        });
    }

    // Mic plane: stream a synthetic 440 Hz tone as the mic uplink (0xCB) — proves client→host
    // mic passthrough end to end without a real microphone (the host decodes it into its virtual
    // source; record that source to hear the tone). Two pacing modes:
    //   default      — Opus 5 ms frames on a steady 5 ms tick (smooth arrival).
    //   --mic-burst  — two 20 ms Opus frames back-to-back every 40 ms, replicating a real
    //                  client's input-tap cadence (the Mac client's AVAudioEngine tap yields
    //                  ~2048-frame buffers → two packets per ~42 ms). This is the arrival
    //                  pattern that exposed the Windows host's missing jitter buffer (constant
    //                  crackle, 2026-07-03): a steady 5 ms stream never trips it. Record the
    //                  host mic and count silence gaps to regression-test host-side buffering.
    #[cfg(not(target_os = "linux"))]
    if args.mic_test {
        tracing::warn!("--mic-test requires Linux (libopus) — skipped");
    }
    #[cfg(target_os = "linux")]
    if args.mic_test {
        let conn2 = conn.clone();
        let burst = args.mic_burst;
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
                        let d = punktfunk_core::quic::encode_mic_datagram(seq, now_ns(), &out[..n]);
                        if conn2.send_datagram(d.into()).is_err() {
                            break 'stream;
                        }
                    }
                    seq = seq.wrapping_add(1);
                }
            }
            tracing::info!("mic-test: done");
        });
    }

    // Touch plane: drag a synthetic finger (touch id 0) in a circle on the client surface, so
    // the host injects it via libei ei_touchscreen — proves the touch path end to end. `flags`
    // packs the surface w/h; x/y are pixels (the host maps them into the device region).
    if args.touch_test {
        let conn2 = conn.clone();
        let (w, h) = (args.mode.width, args.mode.height);
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
                let _ = conn2.send_datagram(
                    touch(InputKind::TouchDown, cx + r, cy)
                        .encode()
                        .to_vec()
                        .into(),
                );
                for i in 0..60u32 {
                    let a = std::f32::consts::TAU * i as f32 / 60.0;
                    let mv = touch(InputKind::TouchMove, cx + r * a.cos(), cy + r * a.sin());
                    let _ = conn2.send_datagram(mv.encode().to_vec().into());
                    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
                }
                let _ = conn2.send_datagram(
                    touch(InputKind::TouchUp, cx + r, cy)
                        .encode()
                        .to_vec()
                        .into(),
                );
                let _ = loop_i;
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
            tracing::info!("touch-test: done");
        });
    }

    // Rich-input plane: instantiate pad 0 on the host (a gamepad event creates the virtual
    // DualSense), then drive its touchpad (drag a finger across) + motion (gyro wobble) over the
    // 0xCC plane. Proves the rich client→host path; the 0xCD feedback is logged by the receive
    // loop below. Requires the host on the DualSense backend (`PUNKTFUNK_GAMEPAD=dualsense`).
    if args.rich_input_test {
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
            let _ = conn2.send_datagram(arrive.encode().to_vec().into());
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
                let _ = conn2.send_datagram(touch(true, 0, 32768).encode().into());
                for i in 0..60u32 {
                    let x = ((i * 65535) / 60) as u16;
                    let _ = conn2.send_datagram(touch(true, x, 32768).encode().into());
                    let g = (((i as i32 % 20) - 10) * 500) as i16; // gyro wobble, ±250 °/s
                    let _ = conn2.send_datagram(
                        RichInput::Motion {
                            pad: 0,
                            gyro: [g, 0, 0],
                            // At rest, gravity is 1 g on +Z — in WIRE units, which are 10000 LSB/g
                            // and not the 8192 or 16384 a particular driver happens to use.
                            accel: [0, 0, MOTION_ACCEL_LSB_PER_G as i16],
                        }
                        .encode()
                        .into(),
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
                }
                let _ = conn2.send_datagram(touch(false, 65535, 32768).encode().into());
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
            tracing::info!("rich-input-test: done");
        });
    }

    // Closed-flag for the blocking receive loop.
    let closed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let closed = closed.clone();
        let conn2 = conn.clone();
        tokio::spawn(async move {
            conn2.closed().await;
            closed.store(true, std::sync::atomic::Ordering::SeqCst);
        });
    }

    // Host→client datagrams: count Opus audio + rumble (playback is the platform clients'
    // job; here we verify the planes flow).
    let audio_pkts = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let audio_bytes = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let rumble_pkts = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let hidout_pkts = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    // Set when a self-terminating v2 rumble envelope (0xCA with the seq+ttl tail) arrives — the
    // Rust-side contract check for `PUNKTFUNK_TEST_FEEDBACK` (asserted at report time). A legacy v1
    // datagram leaves it false, so this only ever fails when a v2 tail we EXPECTED went missing.
    let saw_v2_rumble = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    // Per-AU host timings (0xCF) → the stream loop, which matches them to received AUs by pts
    // and reports the host/network split. try_send: overflow drops samples, never blocks QUIC.
    let (host_timing_tx, host_timing_rx) =
        std::sync::mpsc::sync_channel::<punktfunk_core::quic::HostTiming>(512);
    {
        let (a, ab, r, h) = (
            audio_pkts.clone(),
            audio_bytes.clone(),
            rumble_pkts.clone(),
            hidout_pkts.clone(),
        );
        let saw_v2 = saw_v2_rumble.clone();
        let ht_tx = host_timing_tx;
        let conn2 = conn.clone();
        // Build a multistream decoder for the host-RESOLVED layout so the probe actually decodes
        // the surround stream (not just counts bytes) — the headless validator for the encode path.
        let audio_channels = welcome.audio_channels;
        // The RESOLVED format, off the Welcome — never what was asked for. The host may decline
        // hi-res for any of the reasons in §8.4 and answer Opus, and a probe that assumed its own
        // request would then mis-parse every datagram it was actually sent.
        let audio_codec = welcome.audio_codec;
        let audio_rate_hz = welcome.audio_rate_hz;
        let audio_bits = welcome.audio_bits;
        let audio_out_path = args.audio_out.clone();
        let audio_layout =
            punktfunk_core::audio::AudioLayout::from_wire(welcome.audio_layout).unwrap_or_default();
        tokio::spawn(async move {
            use std::sync::atomic::Ordering::Relaxed;
            let mut hdr_logged = false;
            let mut rumble_logged = false;
            let lossless = audio_codec == punktfunk_core::quic::AUDIO_CODEC_PCM;
            let layout = punktfunk_core::audio::layout_for(audio_channels, audio_layout);
            let mut audio_dec = (!lossless)
                .then(|| {
                    opus::MSDecoder::new(48_000, layout.streams, layout.coupled, layout.mapping)
                        .ok()
                })
                .flatten();
            let mut pcm = vec![0f32; 5760 * audio_channels as usize];
            let mut lossless_pcm: Vec<f32> = Vec::new();
            let mut audio_decoded_logged = false;
            let mut audio_out = audio_out_path.as_deref().and_then(|p| {
                std::fs::File::create(p)
                    .map(std::io::BufWriter::new)
                    .map_err(|e| tracing::error!(path = p, error = %e, "--audio-out did not open"))
                    .ok()
            });
            // Raw interleaved f32 LE, so the reader needs no container — the rate and channel
            // count are on the stats line and in this log.
            let write_pcm = move |samples: &[f32], w: &mut Option<_>| {
                if let Some(f) = w.as_mut() {
                    let mut bytes = Vec::with_capacity(samples.len() * 4);
                    for s in samples {
                        bytes.extend_from_slice(&s.to_le_bytes());
                    }
                    let _ = std::io::Write::write_all(f, &bytes);
                }
            };
            while let Ok(d) = conn2.read_datagram().await {
                if let Some((_, _, pcm_wire)) =
                    punktfunk_core::quic::decode_audio_pcm_datagram(&d).filter(|_| lossless)
                {
                    a.fetch_add(1, Relaxed);
                    ab.fetch_add(pcm_wire.len() as u64, Relaxed);
                    if punktfunk_core::audio::pcm::to_f32(pcm_wire, audio_bits, &mut lossless_pcm)
                        .is_some()
                    {
                        if !audio_decoded_logged {
                            audio_decoded_logged = true;
                            tracing::info!(
                                channels = audio_channels,
                                rate_hz = audio_rate_hz,
                                bits = audio_bits,
                                samples_per_channel =
                                    lossless_pcm.len() / audio_channels.max(1) as usize,
                                "audio decoded (lossless PCM, 0xD3)"
                            );
                        }
                        write_pcm(&lossless_pcm, &mut audio_out);
                    }
                } else if let Some((_, _, opus)) = punktfunk_core::quic::decode_audio_datagram(&d) {
                    a.fetch_add(1, Relaxed);
                    ab.fetch_add(opus.len() as u64, Relaxed);
                    // Decode + validate: the per-channel sample count must be a legal Opus frame
                    // size; log the first success so a loopback test can assert surround decoded.
                    if let Some(dec) = audio_dec.as_mut() {
                        match dec.decode_float(opus, &mut pcm, false) {
                            Ok(samples) if !audio_decoded_logged => {
                                audio_decoded_logged = true;
                                tracing::info!(
                                    channels = audio_channels,
                                    samples_per_channel = samples,
                                    "audio decoded (Opus multistream)"
                                );
                                write_pcm(
                                    &pcm[..samples * audio_channels as usize],
                                    &mut audio_out,
                                );
                            }
                            Ok(samples) => {
                                write_pcm(&pcm[..samples * audio_channels as usize], &mut audio_out)
                            }
                            Err(e) => tracing::debug!(error = %e, "probe audio decode"),
                        }
                    }
                } else if let Some(u) = punktfunk_core::quic::decode_rumble_envelope(&d) {
                    // Log the first rumble so a loopback test can see the self-terminating v2
                    // envelope tail (seq + TTL) arrived, not just the level. `lt`/`rt` are the v3
                    // impulse-trigger levels: printed beside the envelope because the wire-leg
                    // check for trigger rumble is exactly "non-zero lt/rt AND the envelope still
                    // present" — i.e. the trigger tail did not displace the seq/TTL tail.
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
                    // Record that a v2 tail was present — the Rust-side seq/ttl contract check for
                    // PUNKTFUNK_TEST_FEEDBACK (asserted at report time).
                    if u.envelope.is_some() {
                        saw_v2.store(true, Relaxed);
                    }
                    r.fetch_add(1, Relaxed);
                } else if let Some(meta) = punktfunk_core::quic::decode_hdr_meta_datagram(&d) {
                    // HDR static metadata (0xCE). Log the first receipt so a loopback test can
                    // assert the host sent it for an HDR session.
                    if !hdr_logged {
                        hdr_logged = true;
                        tracing::info!(?meta, "HDR static metadata (0xCE)");
                    }
                } else if let Some(hid) = punktfunk_core::quic::HidOutput::decode(&d) {
                    // The DualSense feedback plane (lightbar / player LEDs / adaptive triggers).
                    // Log the first few so a playtest can see triggers/LEDs arrive without spam.
                    if h.fetch_add(1, Relaxed) < 12 {
                        tracing::info!(?hid, "DualSense HID output (0xCD)");
                    }
                } else if let Some(t) = punktfunk_core::quic::decode_host_timing_datagram(&d) {
                    // Per-AU host timing (0xCF) — forwarded to the stream loop for the
                    // host/network latency split.
                    let _ = ht_tx.try_send(t);
                }
            }
        });
    }

    let host_udp = std::net::SocketAddr::new(remote.ip(), welcome.udp_port);
    let cfg = welcome.session_config(Role::Client);
    let expected = welcome.frames;
    let out_path = args.out.clone();
    let (rxp_dt, rxb_dt) = (rx_wire_packets.clone(), rx_wire_bytes.clone());
    let lp_dt = loss_ppm.clone();
    let df_dt = dropped_frames.clone();

    // Express our receive time in the host clock before differencing against the host-stamped
    // capture pts. 0 ⇒ same-host or an old host that didn't answer the skew handshake (the latency
    // is then only valid same-host, as before).
    let clock_offset = clock_offset_ns.unwrap_or(0);
    let skew_corrected = clock_offset_ns.is_some();

    // Data plane on a blocking thread (native threads only on the frame path).
    let result = tokio::task::spawn_blocking(move || -> Result<()> {
        let transport =
            UdpTransport::connect(&format!("0.0.0.0:{udp_port}"), &host_udp.to_string())
                .context("bind data plane")?;
        // Hole-punch the host's data port so video traverses a NAT / inter-VLAN firewall. This
        // tool runs one session then exits, so the keepalive thread dies with the process — no
        // explicit stop needed (the flag is never set).
        if let Ok(sock) = transport.try_clone_socket() {
            let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            punktfunk_core::transport::spawn_data_punch(sock, stop);
        }
        let mut session =
            Session::new(cfg, Box::new(transport)).map_err(|e| anyhow!("client session: {e:?}"))?;
        let mut sink = match &out_path {
            Some(p) => Some(std::io::BufWriter::new(
                std::fs::File::create(p).with_context(|| format!("create {p}"))?,
            )),
            None => None,
        };

        let mut ok = 0u32;
        let mut mismatched = 0u32;
        let mut bytes = 0u64;
        let mut latencies_us: Vec<u64> = Vec::new();
        // Host/network split: received AUs awaiting their 0xCF host timing (pts → capture→received
        // µs), matched as the datagrams arrive. Bounded — an old host never sends any.
        let mut pending_split: std::collections::VecDeque<(u64, u64)> =
            std::collections::VecDeque::new();
        let mut host_us_v: Vec<u64> = Vec::new();
        let mut net_us_v: Vec<u64> = Vec::new();
        // T0.1 host-stage split (extended 0xCF): queue/encode/pace + the derived seal/xfer
        // residual. Empty against a host that predates the stage tail.
        let mut queue_us_v: Vec<u64> = Vec::new();
        let mut enc_us_v: Vec<u64> = Vec::new();
        let mut xfer_us_v: Vec<u64> = Vec::new();
        let mut pace_us_v: Vec<u64> = Vec::new();
        let mut last_rx = std::time::Instant::now();
        let started = std::time::Instant::now();
        // Stream-duration cap: `--seconds N`, else the 120s default. Ending the loop here reaches the
        // graceful `conn.close` below (with the deliberate-quit code if `--quit`).
        let cap_secs = args.seconds.unwrap_or(120);
        // Adaptive-FEC loss window: publish a fresh estimate every 750 ms for the LossReport task.
        let mut last_loss_report = std::time::Instant::now();
        let (mut last_recovered, mut last_late, mut last_received) = (0u64, 0u64, 0u64);
        loop {
            // Mirror packet-level receive counters for the speed-test reporter (reads their delta),
            // and publish a windowed loss estimate for the adaptive-FEC LossReport task.
            {
                use std::sync::atomic::Ordering::Relaxed;
                let s = session.stats();
                rxp_dt.store(s.packets_received, Relaxed);
                rxb_dt.store(s.bytes_received, Relaxed);
                df_dt.store(s.frames_dropped, Relaxed);
                if last_loss_report.elapsed() >= std::time::Duration::from_millis(750) {
                    lp_dt.store(
                        window_loss_ppm(
                            s.fec_recovered_shards.wrapping_sub(last_recovered),
                            s.fec_late_shards.wrapping_sub(last_late),
                            s.packets_received.wrapping_sub(last_received),
                        ),
                        Relaxed,
                    );
                    last_loss_report = std::time::Instant::now();
                    last_recovered = s.fec_recovered_shards;
                    last_late = s.fec_late_shards;
                    last_received = s.packets_received;
                }
            }
            if expected > 0 && ok + mismatched >= expected {
                break;
            }
            if closed.load(std::sync::atomic::Ordering::SeqCst)
                && last_rx.elapsed() > std::time::Duration::from_millis(300)
            {
                break;
            }
            if started.elapsed() > std::time::Duration::from_secs(cap_secs)
                || last_rx.elapsed() > std::time::Duration::from_secs(45)
            {
                break;
            }
            match session.poll_frame() {
                Ok(frame) => {
                    last_rx = std::time::Instant::now();
                    // Speed-test filler isn't video: it's measured via the packet-level counters
                    // mirrored at the loop head — skip verification / the --out sink.
                    if frame.flags & FLAG_PROBE as u32 != 0 {
                        continue;
                    }
                    bytes += frame.data.len() as u64;
                    // capture→received: our receive instant in the host clock (now + offset)
                    // minus the host's capture pts. offset is 0 same-host / old host.
                    let lat = (now_ns() as i128 + clock_offset as i128 - frame.pts_ns as i128)
                        .max(0) as u64;
                    if lat > 0 && lat < 10_000_000_000 {
                        latencies_us.push(lat / 1000);
                        pending_split.push_back((frame.pts_ns, lat / 1000));
                        if pending_split.len() > 1024 {
                            pending_split.pop_front();
                        }
                    }
                    // Match any host timings (0xCF) that have arrived: host = the reported
                    // capture→sent, network = our capture→received minus it (per-frame tiling).
                    while let Ok(t) = host_timing_rx.try_recv() {
                        if let Some(i) = pending_split.iter().position(|(p, _)| *p == t.pts_ns) {
                            let (_, hostnet_us) = pending_split.remove(i).unwrap();
                            host_us_v.push(t.host_us as u64);
                            net_us_v.push(hostnet_us.saturating_sub(t.host_us as u64));
                            if let Some(s) = t.stages {
                                queue_us_v.push(s.queue_us as u64);
                                enc_us_v.push(s.encode_us as u64);
                                pace_us_v.push(s.pace_us as u64);
                                xfer_us_v.push((t.host_us as u64).saturating_sub(
                                    s.queue_us as u64 + s.encode_us as u64 + s.pace_us as u64,
                                ));
                            }
                        }
                    }
                    if expected > 0 {
                        // Verification mode: deterministic content.
                        let idx = u32::from_le_bytes(frame.data[0..4].try_into().unwrap());
                        if frame.data == test_frame(idx, frame.data.len()) {
                            ok += 1;
                        } else {
                            mismatched += 1;
                        }
                    } else {
                        ok += 1;
                        if let Some(s) = sink.as_mut() {
                            s.write_all(&frame.data).context("write AU")?;
                        }
                    }
                }
                Err(PunktfunkError::NoFrame) => {
                    std::thread::sleep(std::time::Duration::from_micros(300));
                }
                Err(e) => return Err(anyhow!("poll_frame: {e:?}")),
            }
        }
        if let Some(mut s) = sink {
            s.flush().ok();
        }

        // PUNKTFUNK_PERF: cumulative receive-path stage split for the whole run — where the
        // receive core's time went (kernel drain vs AES-GCM open vs reassembly+FEC). This is
        // the measurement tool's view of the client-pump wall the 2026-07-14 sweeps pinned.
        if let Some(p) = session.take_pump_perf() {
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

        latencies_us.sort_unstable();
        let pct = |p: f64| -> u64 {
            if latencies_us.is_empty() {
                return 0;
            }
            let i = ((latencies_us.len() as f64 * p) as usize).min(latencies_us.len() - 1);
            latencies_us[i]
        };
        tracing::info!(
            frames = ok,
            mismatched,
            mb = bytes / 1_000_000,
            lat_p50_us = pct(0.50),
            lat_p95_us = pct(0.95),
            lat_p99_us = pct(0.99),
            lat_max_us = latencies_us.last().copied().unwrap_or(0),
            skew_corrected,
            "punktfunk/1 stream complete (capture→received latency; skew_corrected=true ⇒ \
             cross-machine valid, false ⇒ same-host clock)"
        );
        if !host_us_v.is_empty() {
            // The host/network split from the per-AU 0xCF timings (design/stats-unification.md
            // Phase 2): host = the host's own capture→sent, network = capture→received minus it.
            let pcts = |v: &mut Vec<u64>, p: f64| -> u64 {
                if v.is_empty() {
                    return 0;
                }
                v.sort_unstable();
                v[((v.len() as f64 * p) as usize).min(v.len() - 1)]
            };
            tracing::info!(
                timing_samples = host_us_v.len(),
                host_p50_us = pcts(&mut host_us_v, 0.50),
                host_p95_us = pcts(&mut host_us_v, 0.95),
                net_p50_us = pcts(&mut net_us_v, 0.50),
                net_p95_us = pcts(&mut net_us_v, 0.95),
                "host/network latency split (host = capture→sent on the host; network = wire + \
                 reassembly)"
            );
            if !queue_us_v.is_empty() {
                // The T0.1 per-stage host attribution: queue (capture→submit) → encode
                // (submit→bitstream) → xfer (seal/FEC + send-channel wait, derived residual)
                // → pace (the microburst spread). The four tile host_us per frame.
                tracing::info!(
                    stage_samples = queue_us_v.len(),
                    queue_p50_us = pcts(&mut queue_us_v, 0.50),
                    queue_p95_us = pcts(&mut queue_us_v, 0.95),
                    encode_p50_us = pcts(&mut enc_us_v, 0.50),
                    encode_p95_us = pcts(&mut enc_us_v, 0.95),
                    xfer_p50_us = pcts(&mut xfer_us_v, 0.50),
                    xfer_p95_us = pcts(&mut xfer_us_v, 0.95),
                    pace_p50_us = pcts(&mut pace_us_v, 0.50),
                    pace_p95_us = pcts(&mut pace_us_v, 0.95),
                    "host stage split (queue → encode → xfer → pace tile the host figure)"
                );
            }
        } else {
            tracing::info!("no host timing datagrams (0xCF) — old host; host+network unsplit");
        }
        if expected > 0 {
            anyhow::ensure!(mismatched == 0, "{mismatched} corrupted frames");
            anyhow::ensure!(ok == expected, "received {ok}/{expected} frames");
            tracing::info!("verification PASSED");
        } else {
            anyhow::ensure!(ok > 0, "no frames received");
        }
        Ok(())
    })
    .await?;

    // Report the side planes whether or not the video plane succeeded.
    {
        use std::sync::atomic::Ordering::Relaxed;
        let (a, ab, r, h) = (
            audio_pkts.load(Relaxed),
            audio_bytes.load(Relaxed),
            rumble_pkts.load(Relaxed),
            hidout_pkts.load(Relaxed),
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
    }

    // Rust-side rumble-envelope contract check: when the host was told to script a feedback burst
    // (PUNKTFUNK_TEST_FEEDBACK, shared by a loopback harness), fail if no self-terminating v2 tail
    // (seq + TTL) arrived — a regression that reverted the host to v1 level datagrams increments the
    // rumble counter identically and would otherwise pass silently. Only tightens an otherwise-OK run
    // (a video failure stays the primary error). The level + hidout planes are asserted end-to-end by
    // the Apple loopback; this covers the seq/ttl tail on the Rust/Linux path.
    let result = if std::env::var("PUNKTFUNK_TEST_FEEDBACK").as_deref() == Ok("1")
        && result.is_ok()
        && !saw_v2_rumble.load(std::sync::atomic::Ordering::Relaxed)
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

/// The host's deterministic test frame (mirror of `punktfunk-host::m3::test_frame`).
fn test_frame(idx: u32, len: usize) -> Vec<u8> {
    let mut d = vec![0u8; len];
    if len >= 4 {
        d[0..4].copy_from_slice(&idx.to_le_bytes());
    }
    for (i, b) in d.iter_mut().enumerate().skip(4) {
        *b = (idx as u8).wrapping_add(i as u8);
    }
    d
}
