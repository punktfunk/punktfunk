//! Opens the macOS desktop-audio capture for three seconds and prints what it delivered:
//! the backend's own log lines (tap rate and format), then frames and peak level.
//!
//! Never muted: it refuses to run unless `PUNKTFUNK_AUDIO_OUTPUT_MODE=host_and_client`, so a
//! listener on this Mac hears nothing change. The tap needs the System Audio Recording
//! permission for the process that owns this terminal; without it the tap delivers digital
//! silence.

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("mac_audio_probe runs on macOS");
}

#[cfg(target_os = "macos")]
fn main() -> anyhow::Result<()> {
    use std::time::{Duration, Instant};

    anyhow::ensure!(
        pf_host_config::config()
            .audio_output_mode
            .prefers_host_hardware(),
        "run the probe with PUNKTFUNK_AUDIO_OUTPUT_MODE=host_and_client: it must not mute"
    );
    tracing_subscriber::fmt().init();

    println!("rate probe: {:?}", pf_audio::probe_capture_rate());
    println!("playing apps: {:?}", pf_audio::playing_apps());
    let mut cap = pf_audio::open_audio_capture(2, pf_audio::SAMPLE_RATE)?;
    let (ch, hz) = (cap.channels() as usize, cap.sample_rate());
    let start = Instant::now();
    let (mut frames, mut chunks, mut peak) = (0usize, 0usize, 0f32);
    while start.elapsed() < Duration::from_secs(3) {
        let chunk = cap.next_chunk_within(Duration::from_millis(100))?;
        frames += chunk.len() / ch;
        chunks += usize::from(!chunk.is_empty());
        peak = chunk.iter().fold(peak, |m, s| m.max(s.abs()));
    }
    let secs = start.elapsed().as_secs_f64();
    drop(cap);
    println!(
        "{ch} ch at {hz} Hz: {frames} frames in {chunks} chunks over {secs:.2} s \
         ({:.0} % of real time), peak {peak:.4}",
        frames as f64 / (secs * f64::from(hz)) * 100.0
    );
    Ok(())
}
