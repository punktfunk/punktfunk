//! Whether the video engine's picture still moves. Twice a second at 60 Hz it copies the
//! convert's source and its output into staging textures, reads them back one sample later
//! (the GPU is long done, so the read never waits) and hashes a sparse grid of each. An output
//! that stands still while its source moves is a stale convert; both still is a still desktop.
//!
//! One `[pf-vd] content:` line per 10 s, plus a line when the output stops and starts again.

use std::time::{Duration, Instant};

use windows62::Win32::Graphics::Direct3D11 as d3d;

type Tex = d3d::ID3D11Texture2D;

/// Frames between samples.
const EVERY: u64 = 30;
/// Consecutive stuck samples before the stall is logged: 2 s at 60 Hz.
const STUCK_SAMPLES: u32 = 4;
const REPORT: Duration = Duration::from_secs(10);
/// Every 8th row, every 16th byte: enough to see a moving picture, cheap to hash.
const ROW_STEP: usize = 8;
const BYTE_STEP: usize = 16;

/// What one sample changed about the stall state.
#[derive(Debug, PartialEq, Eq)]
pub enum Event {
    /// The output has not moved for this many samples while its source did.
    Stuck(u32),
    /// The output moved again after a logged stall of this many samples.
    Moving(u32),
}

/// The per-sample bookkeeping, apart from the GPU: pure, so it has a test.
#[derive(Default)]
pub struct Tally {
    last: Option<(u64, u64)>,
    pub samples: u32,
    pub src_moved: u32,
    pub out_moved: u32,
    pub stuck: u32,
    run: u32,
    logged: bool,
}

impl Tally {
    /// Record one sample's hashes; `Some` when a stall starts or ends.
    pub fn note(&mut self, src: u64, out: u64) -> Option<Event> {
        let (last_src, last_out) = self.last.replace((src, out))?;
        self.samples += 1;
        let src_moved = src != last_src;
        let out_moved = out != last_out;
        self.src_moved += u32::from(src_moved);
        self.out_moved += u32::from(out_moved);
        if src_moved && !out_moved {
            self.stuck += 1;
            self.run += 1;
            if self.run >= STUCK_SAMPLES && !self.logged {
                self.logged = true;
                return Some(Event::Stuck(self.run));
            }
        } else if out_moved {
            let run = std::mem::take(&mut self.run);
            if std::mem::take(&mut self.logged) {
                return Some(Event::Moving(run));
            }
        }
        None
    }

    /// Start the next report window; the stall state carries over.
    fn reset_window(&mut self) {
        self.samples = 0;
        self.src_moved = 0;
        self.out_moved = 0;
        self.stuck = 0;
    }
}

pub struct ContentProbe {
    label: &'static str,
    frames: u64,
    staging: Option<(Tex, Tex)>,
    pending: bool,
    tally: Tally,
    since: Instant,
}

impl ContentProbe {
    pub fn new(label: &'static str) -> Self {
        Self {
            label,
            frames: 0,
            staging: None,
            pending: false,
            tally: Tally::default(),
            since: Instant::now(),
        }
    }

    /// Call right after the convert wrote `out` from `src`, with the context lock held. Most
    /// calls return at once; a sampling call reads the previous copies and queues new ones.
    pub fn after_convert(
        &mut self,
        dev: &d3d::ID3D11Device,
        ctx: &d3d::ID3D11DeviceContext,
        src: &Tex,
        out: &Tex,
    ) {
        self.frames += 1;
        if !self.frames.is_multiple_of(EVERY) {
            return;
        }
        if self.pending {
            match self.read(ctx) {
                Some((s, o)) => self.record(s, o),
                // Still drawing: keep the copies and try again next sample.
                None => return,
            }
        }
        let Some((src_stage, out_stage)) = self.stage(dev, src, out) else {
            return;
        };
        // SAFETY: same-size, same-format textures on one device (checked in `stage`); the
        // caller holds the context lock, so the two copies queue right behind the convert.
        unsafe {
            ctx.CopyResource(&src_stage, src);
            ctx.CopyResource(&out_stage, out);
        }
        self.pending = true;
        if self.since.elapsed() >= REPORT {
            let t = &self.tally;
            dbglog!(
                "[pf-vd] content: kind={} samples={} src_moved={} out_moved={} stuck={}",
                self.label,
                t.samples,
                t.src_moved,
                t.out_moved,
                t.stuck
            );
            self.tally.reset_window();
            self.since = Instant::now();
        }
    }

    fn record(&mut self, src: u64, out: u64) {
        let secs = |n: u32| f64::from(n) * EVERY as f64 / 60.0;
        match self.tally.note(src, out) {
            Some(Event::Stuck(n)) => dbglog!(
                "[pf-vd] content: kind={} converted picture stood still for {:.1} s while its source moved",
                self.label,
                secs(n)
            ),
            Some(Event::Moving(n)) => dbglog!(
                "[pf-vd] content: kind={} converted picture moves again after {:.1} s",
                self.label,
                secs(n)
            ),
            None => {}
        }
    }

    /// Staging copies of `src` and `out`, rebuilt when either changes shape.
    fn stage(&mut self, dev: &d3d::ID3D11Device, src: &Tex, out: &Tex) -> Option<(Tex, Tex)> {
        let want = (desc(src), desc(out));
        if let Some((s, o)) = &self.staging
            && same_shape(&desc(s), &want.0)
            && same_shape(&desc(o), &want.1)
        {
            return Some((s.clone(), o.clone()));
        }
        self.pending = false;
        self.tally.last = None;
        let pair = (staging_like(dev, &want.0)?, staging_like(dev, &want.1)?);
        self.staging = Some(pair.clone());
        Some(pair)
    }

    /// Hashes of the pending copies; `None` while the GPU still owns them.
    fn read(&mut self, ctx: &d3d::ID3D11DeviceContext) -> Option<(u64, u64)> {
        let (s, o) = self.staging.as_ref()?;
        let src = map_hash(ctx, s)?;
        let out = map_hash(ctx, o)?;
        self.pending = false;
        Some((src, out))
    }
}

fn desc(t: &Tex) -> d3d::D3D11_TEXTURE2D_DESC {
    let mut d = d3d::D3D11_TEXTURE2D_DESC::default();
    // SAFETY: `t` is a live texture; `d` a valid local out-param.
    unsafe { t.GetDesc(&mut d) };
    d
}

fn same_shape(a: &d3d::D3D11_TEXTURE2D_DESC, b: &d3d::D3D11_TEXTURE2D_DESC) -> bool {
    (a.Width, a.Height, a.Format) == (b.Width, b.Height, b.Format)
}

fn staging_like(dev: &d3d::ID3D11Device, like: &d3d::D3D11_TEXTURE2D_DESC) -> Option<Tex> {
    let desc = d3d::D3D11_TEXTURE2D_DESC {
        MipLevels: 1,
        ArraySize: 1,
        Usage: d3d::D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: d3d::D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
        ..*like
    };
    let mut t: Option<Tex> = None;
    // SAFETY: `desc` is a fully-initialized local; `t` a valid out-param checked below.
    let hr = unsafe { dev.CreateTexture2D(&desc, None, Some(&mut t)) };
    if hr.is_err() || t.is_none() {
        dbglog!(
            "[pf-vd] content: staging texture ({:?}) failed: {hr:?}",
            like.Format
        );
    }
    t
}

/// FNV-1a over a sparse grid of the texture's first plane. `None` while the copy is in flight.
fn map_hash(ctx: &d3d::ID3D11DeviceContext, t: &Tex) -> Option<u64> {
    let d = desc(t);
    let mut m = d3d::D3D11_MAPPED_SUBRESOURCE::default();
    // SAFETY: `t` is our staging texture; DO_NOT_WAIT returns WAS_STILL_DRAWING instead of
    // blocking, and a failed map leaves nothing to unmap.
    unsafe {
        ctx.Map(
            t,
            0,
            d3d::D3D11_MAP_READ,
            d3d::D3D11_MAP_FLAG_DO_NOT_WAIT.0 as u32,
            Some(&mut m),
        )
    }
    .ok()?;
    let pitch = m.RowPitch as usize;
    let row_bytes = pitch.min(d.Width as usize * 4);
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for row in (0..d.Height as usize).step_by(ROW_STEP) {
        // SAFETY: the mapping spans at least `Height` rows of `RowPitch` bytes for the first
        // plane, and `row_bytes` never exceeds the pitch.
        let line = unsafe {
            std::slice::from_raw_parts((m.pData as *const u8).add(row * pitch), row_bytes)
        };
        for &b in line.iter().step_by(BYTE_STEP) {
            h = (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
        }
    }
    // SAFETY: the subresource mapped above.
    unsafe { ctx.Unmap(t, 0) };
    Some(h)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_still_output_under_a_moving_source_is_logged_once_and_its_end_too() {
        let mut t = Tally::default();
        assert_eq!(
            t.note(1, 1),
            None,
            "the first sample has nothing to compare"
        );
        for i in 2..=4 {
            assert_eq!(t.note(i, 1), None);
        }
        assert_eq!(t.note(5, 1), Some(Event::Stuck(4)));
        assert_eq!(t.note(6, 1), None, "a stall is logged once");
        assert_eq!(t.note(7, 2), Some(Event::Moving(5)));
        assert_eq!((t.samples, t.src_moved, t.out_moved, t.stuck), (6, 6, 1, 5));
    }

    #[test]
    fn a_still_desktop_is_not_a_stall() {
        let mut t = Tally::default();
        for _ in 0..10 {
            assert_eq!(t.note(9, 9), None);
        }
        assert_eq!(t.stuck, 0);
    }
}
