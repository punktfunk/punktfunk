//! Captures the main display through `pf_capture::open_display` for three seconds at 60 Hz and
//! prints what arrived: frames, the buffer's format and size, time to the first frame, the rows
//! the display fills, and the pointer when asked. The capturer's closing log line counts the
//! idle frames. It needs the Screen Recording grant for the process that runs it.
//!
//! `cargo run -p pf-capture --example mac_capture_probe -- [WxH] [hdr|10bit|444|full] [cursor]`

#[cfg(target_os = "macos")]
fn main() -> anyhow::Result<()> {
    probe::run()
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("mac_capture_probe runs on macOS only");
}

#[cfg(target_os = "macos")]
mod probe {
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use anyhow::{bail, Context};
    use objc2_core_video::{
        CVPixelBuffer, CVPixelBufferGetBaseAddressOfPlane, CVPixelBufferGetBytesPerRowOfPlane,
        CVPixelBufferGetHeight, CVPixelBufferGetPixelFormatType, CVPixelBufferLockBaseAddress,
        CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
    };
    use pf_capture::{DisplayCaptureOpts, FirstFrame};
    use pf_frame::{FramePayload, OutputFormat, PixelFormat};

    const RUN: Duration = Duration::from_secs(3);
    const HZ: u32 = 60;

    pub fn run() -> anyhow::Result<()> {
        tracing_subscriber::fmt().with_target(false).init();
        if !pf_capture::screen_capture_access() {
            bail!(
                "no Screen Recording grant for this process: grant it to the app that started \
                 it (System Settings, Privacy & Security, Screen & System Audio Recording)"
            );
        }
        let args: Vec<String> = std::env::args().skip(1).collect();
        let has = |a: &str| args.iter().any(|x| x == a);
        let (width, height) = args
            .iter()
            .find_map(|a| a.split_once('x'))
            .map(|(w, h)| Ok::<_, anyhow::Error>((w.parse()?, h.parse()?)))
            .transpose()
            .context("parse WxH")?
            .unwrap_or((1920, 1080));
        let output = OutputFormat {
            ten_bit_sdr: has("10bit"),
            chroma_444: has("444"),
            full_range: has("full"),
            ..OutputFormat::resolve(has("hdr"), true)
        };
        let display_id = objc2_core_graphics::CGMainDisplayID();
        let t0 = Instant::now();
        let mut cap = pf_capture::open_display(DisplayCaptureOpts {
            display_id,
            width,
            height,
            stream_hz: HZ,
            output,
            metadata_cursor: has("cursor"),
            keepalive: Box::new(()),
        })?;
        let opened = t0.elapsed();
        let first = cap.next_frame(FirstFrame::Default)?;
        println!(
            "display {display_id} at {width}x{height} {HZ} Hz: opened in {opened:.1?}, first \
             frame at {:.1?}",
            t0.elapsed()
        );
        describe(&first);
        println!("hdr metadata: {}", cap.colour().hdr_meta.is_some());

        let (mut frames, mut ages, mut cursor_us) = (1u32, Vec::new(), Vec::new());
        let (mut overlay, mut shown, mut serials) = (None, 0, std::collections::BTreeSet::new());
        let end = Instant::now() + RUN;
        while Instant::now() < end {
            let tick = (Instant::now() + Duration::from_millis(16)).min(end);
            cap.feed().context("no feed")?.wait_arrival(tick);
            if let Some(f) = cap.try_latest()? {
                frames += 1;
                ages.push(now_ns().saturating_sub(f.pts_ns) / 1000);
            }
            if let Some(c) = cap.cursor() {
                let t = Instant::now();
                overlay = c.overlay();
                cursor_us.push(t.elapsed().as_micros() as u64);
                if let Some(o) = &overlay {
                    shown += u32::from(o.visible);
                    serials.insert(o.serial);
                }
            }
        }
        ages.sort_unstable();
        println!(
            "{frames} complete frames in {RUN:?} (the first included); capture age at receipt \
             p50 {} us, max {} us",
            ages.get(ages.len() / 2).copied().unwrap_or(0),
            ages.last().copied().unwrap_or(0)
        );
        if let Some(o) = overlay {
            cursor_us.sort_unstable();
            println!(
                "cursor: {}x{} at ({}, {}), hotspot ({}, {}); visible in {shown} of {} calls, \
                 {} shapes; overlay() p50 {} us, max {} us",
                o.w,
                o.h,
                o.x,
                o.y,
                o.hot_x,
                o.hot_y,
                cursor_us.len(),
                serials.len(),
                cursor_us[cursor_us.len() / 2],
                cursor_us.last().copied().unwrap_or(0),
            );
        } else if has("cursor") {
            println!("cursor: none (the frames carry the pointer)");
        }
        Ok(())
    }

    fn now_ns() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0)
    }

    fn describe(frame: &pf_frame::CapturedFrame) {
        let FramePayload::PixelBuffer(pb) = &frame.payload else {
            println!("payload is not a pixel buffer");
            return;
        };
        let code = CVPixelBufferGetPixelFormatType(&pb.0);
        println!(
            "format {:?} ({}), {}x{}, pts age {} us",
            frame.format,
            String::from_utf8_lossy(&code.to_be_bytes()),
            frame.width,
            frame.height,
            now_ns().saturating_sub(frame.pts_ns) / 1000
        );
        if let Some((top, bottom)) = lit_rows(&pb.0, frame.format) {
            println!("rows with picture in the middle column: {top}..={bottom}");
        }
    }

    /// The first and last rows of the middle column brighter than black: the letterbox.
    fn lit_rows(pb: &CVPixelBuffer, format: PixelFormat) -> Option<(usize, usize)> {
        let lock = CVPixelBufferLockFlags::ReadOnly;
        // SAFETY: a live buffer, locked read-only for the reads below and unlocked after.
        if unsafe { CVPixelBufferLockBaseAddress(pb, lock) } != 0 {
            return None;
        }
        let base = CVPixelBufferGetBaseAddressOfPlane(pb, 0).cast::<u8>();
        let stride = CVPixelBufferGetBytesPerRowOfPlane(pb, 0);
        let rows = CVPixelBufferGetHeight(pb);
        let lit = |row: usize| {
            let at = row * stride + stride / 2;
            // SAFETY: `at` is inside plane 0 (`rows` rows of `stride` bytes), which stays
            // mapped while the buffer is locked.
            let px = |i: usize| unsafe { *base.add(at + i) };
            match format {
                PixelFormat::Nv12 => px(0) > 24,
                PixelFormat::P010 => u16::from_le_bytes([px(0) & 0xfe, px(1)]) >> 6 > 96,
                _ => px(0).max(px(1)).max(px(2)) > 8,
            }
        };
        let found = (!base.is_null()).then(|| {
            let top = (0..rows).find(|&r| lit(r));
            let bottom = (0..rows).rev().find(|&r| lit(r));
            top.zip(bottom)
        });
        // SAFETY: the lock taken above.
        unsafe { CVPixelBufferUnlockBaseAddress(pb, lock) };
        found.flatten()
    }
}
