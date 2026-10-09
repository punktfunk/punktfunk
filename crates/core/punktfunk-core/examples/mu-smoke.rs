//! Headless gamescope multi-user isolation smoke (`design/gamescope-multiuser.md`).
//!
//! Pins `CompositorPref::Gamescope` (isolated Spawn — Auto shares the seat) and
//! prints frame/audio counts. `--input` moves the pointer so the session's pinned
//! injector opens on its per-session EIS relay. No decode, no presentation.
//!
//! Usage: `mu-smoke <port> <W> <H> [seconds] [--input]`

use punktfunk_core::client::{ConnectParams, NativeClient};
use punktfunk_core::config::CompositorPref;
use punktfunk_core::input::{InputEvent, InputKind};
use punktfunk_core::{Mode, PunktfunkError};
use std::time::{Duration, Instant};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let port: u16 = args[1].parse().expect("port");
    let w: u32 = args[2].parse().expect("width");
    let h: u32 = args[3].parse().expect("height");
    let secs: u64 = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(8);
    let send_input = args.iter().any(|a| a == "--input");

    let mode = Mode {
        width: w,
        height: h,
        refresh_hz: 60,
    };
    let client = NativeClient::connect(ConnectParams {
        compositor: CompositorPref::Gamescope,
        name: Some(format!("mu-smoke-{w}x{h}")),
        ..ConnectParams::new("127.0.0.1", port, mode, Duration::from_secs(40))
    })
    .expect("connect");
    eprintln!("mu-smoke {w}x{h}: connected");

    let (mut frames, mut audio_pkts, mut audio_bytes) = (0u64, 0u64, 0u64);
    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut next_wiggle = Instant::now();
    while Instant::now() < deadline {
        match client.next_frame(Duration::from_millis(50)) {
            Ok(_) => frames += 1,
            Err(PunktfunkError::NoFrame) => {}
            Err(e) => {
                eprintln!("mu-smoke {w}x{h}: session ended: {e:?}");
                break;
            }
        }
        while let Ok(pkt) = client.next_audio(Duration::ZERO) {
            audio_pkts += 1;
            audio_bytes += pkt.data.len() as u64;
        }
        if send_input && Instant::now() >= next_wiggle {
            for dx in [3i32, -3] {
                let _ = client.send_input(&InputEvent {
                    kind: InputKind::MouseMove,
                    _pad: [0; 3],
                    code: 0,
                    x: dx,
                    y: 0,
                    flags: 0,
                });
            }
            next_wiggle = Instant::now() + Duration::from_millis(500);
        }
    }
    println!(
        "RESULT {w}x{h} frames={frames} audio_pkts={audio_pkts} audio_avg_bytes={}",
        audio_bytes.checked_div(audio_pkts).unwrap_or(0)
    );
}
