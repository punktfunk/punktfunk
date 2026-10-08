//! What the wave smokes share: the moving texture they encode, and the writer that stores a
//! stream the way `gpu_parity`'s field hashers read it. Rows band by luma and columns band the green,
//! so motion in either axis codes; `PF_WAVE_SCROLL=dx,dy` moves it that many pixels per
//! frame (default down 6, so motion vectors point up into rows a sweep already refreshed);
//! `PF_WAVE_NOISE=1` adds per-pixel noise that scrolls with the content, which starves the
//! encoder the way game content does. Test-only callers; the drivers below build only for
//! tests or the `test-support` feature, and the D3D11 frames live in `pf-encode-win`.

/// BGRA, `w * h * 4` bytes, at `frame` frames of motion.
pub fn scroll_pattern(w: usize, h: usize, frame: usize) -> Vec<u8> {
    let (dx, dy) = std::env::var("PF_WAVE_SCROLL")
        .ok()
        .and_then(|s| {
            let (x, y) = s.split_once(',')?;
            Some((x.trim().parse::<i64>().ok()?, y.trim().parse::<i64>().ok()?))
        })
        .unwrap_or((0, 6));
    let (sx, sy) = (dx * frame as i64, dy * frame as i64);
    let noise = std::env::var("PF_WAVE_NOISE").is_ok_and(|v| v == "1");
    let mut px = vec![0u8; w * h * 4];
    for y in 0..h {
        let cy = (y as i64 - sy).rem_euclid(h as i64);
        let band = cy as u8;
        for x in 0..w {
            let cx = (x as i64 - sx).rem_euclid(w as i64);
            let col = cx as u8;
            let o = (y * w + x) * 4;
            let n = if noise {
                // xorshift of the content coordinate: white noise, ±32 per channel.
                let mut v = (cx as u32).wrapping_mul(0x9E37_79B9)
                    ^ (cy as u32).wrapping_mul(0x85EB_CA6B)
                    ^ 0x5bd1_e995;
                v ^= v << 13;
                v ^= v >> 17;
                v ^= v << 5;
                (v & 63) as i16 - 32
            } else {
                0
            };
            let c = |b: u8| (i16::from(b) + n).clamp(0, 255) as u8;
            px[o] = c(band.wrapping_mul(3));
            px[o + 1] = c(band ^ col);
            px[o + 2] = c(255 - band);
            px[o + 3] = 255;
        }
    }
    px
}

/// The pattern as NV12, `w * h * 3 / 2` bytes: BT.601 limited range, chroma from the top-left
/// pixel of each 2x2 block. For encoders that take no BGRA.
pub fn scroll_pattern_nv12(w: usize, h: usize, frame: usize) -> Vec<u8> {
    let bgra = scroll_pattern(w, h, frame);
    let rgb = |x: usize, y: usize| {
        let o = (y * w + x) * 4;
        let c = |k: usize| i32::from(bgra[o + k]);
        (c(2), c(1), c(0))
    };
    let mut nv12 = vec![0u8; w * h * 3 / 2];
    for y in 0..h {
        for x in 0..w {
            let (r, g, b) = rgb(x, y);
            nv12[y * w + x] = (((66 * r + 129 * g + 25 * b + 128) >> 8) + 16) as u8;
        }
    }
    for y in 0..h / 2 {
        for x in 0..w / 2 {
            let (r, g, b) = rgb(2 * x, 2 * y);
            let o = w * h + y * w + 2 * x;
            nv12[o] = (((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128) as u8;
            nv12[o + 1] = (((112 * r - 94 * g - 18 * b + 128) >> 8) + 128) as u8;
        }
    }
    nv12
}

/// Write `aus` to `path` with the `.idx` sidecar a `PUNKTFUNK_DUMP_VIDEO` capture carries
/// (`offset len flags complete` per access unit), so a field hasher splits any codec's stream
/// by access unit.
pub fn write_capture(path: &str, aus: &[&[u8]]) -> std::io::Result<()> {
    let mut data = Vec::new();
    let mut idx = String::new();
    for au in aus {
        idx.push_str(&format!("{} {} 0x0 1\n", data.len(), au.len()));
        data.extend_from_slice(au);
    }
    std::fs::write(path, &data)?;
    std::fs::write(format!("{path}.idx"), idx)
}

/// The LTR anchor soak both slot-RFI backends run on hardware, from the environment:
/// `PF_WAVE_SMOKE=WxH[:8[:fps[:mbps]]]` (NV12 only, default `256x256:8:60:2`), `PF_WAVE_SOAK`
/// losses (12), one every `PF_WAVE_GAP` frames (40) asked about `PF_WAVE_LAG` frames later (2),
/// and `PF_WAVE_CODEC=h264` or `av1` over HEVC.
#[cfg(any(test, feature = "test-support"))]
pub struct Soak {
    pub w: u32,
    pub h: u32,
    pub fps: u32,
    pub mbps: u64,
    pub codec: crate::Codec,
    losses: usize,
    gap: usize,
    lag: usize,
    /// `PF_WAVE_ACKED=1`: nothing is asked; before each frame the encoder takes the newest
    /// frame `lag` or more back that was not lost as its reference floor, as a client's
    /// confirmations give it.
    acked: bool,
}

#[cfg(any(test, feature = "test-support"))]
impl Soak {
    pub fn from_env() -> Soak {
        let shape = std::env::var("PF_WAVE_SMOKE").unwrap_or_else(|_| "256x256:8:60:2".into());
        let mut parts = shape.split(':');
        let (w, h) = parts
            .next()
            .and_then(|s| s.split_once('x'))
            .map(|(w, h)| (w.parse::<u32>().unwrap(), h.parse::<u32>().unwrap()))
            .expect("PF_WAVE_SMOKE=WxH[:8[:fps[:mbps]]]");
        assert_ne!(parts.next(), Some("10"), "the soak feeds NV12");
        let count = |k: &str, d: usize| {
            std::env::var(k)
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(d)
        };
        let soak = Soak {
            w,
            h,
            fps: parts.next().map_or(60, |f| f.parse().unwrap()),
            mbps: parts.next().map_or(2, |m| m.parse().unwrap()),
            codec: match std::env::var("PF_WAVE_CODEC").as_deref() {
                Ok("h264") => crate::Codec::H264,
                Ok("av1") => crate::Codec::Av1,
                _ => crate::Codec::H265,
            },
            losses: count("PF_WAVE_SOAK", 12),
            gap: count("PF_WAVE_GAP", 40),
            lag: count("PF_WAVE_LAG", 2),
            acked: std::env::var("PF_WAVE_ACKED").is_ok_and(|v| v == "1"),
        };
        assert!(
            soak.lag >= 1 && soak.lag < soak.gap,
            "PF_WAVE_LAG=1..PF_WAVE_GAP"
        );
        soak
    }

    /// Drive `enc` with `frame(i)` for every frame index and check each loss came back as a
    /// recovery anchor or, declined, as an IDR; under `acked`, that the frame after each loss
    /// leans on a confirmed one. The full stream and the view without the lost frames land in
    /// `PUNKTFUNK_SMOKE_DIR` as `{name}-{anchor,acked}.*` and `…-dropS.*`, with the `.idx`
    /// sidecars `gpu_parity`'s field hashers read.
    pub fn run(
        &self,
        name: &str,
        enc: &mut dyn crate::Encoder,
        mut frame: impl FnMut(usize) -> pf_frame::CapturedFrame,
    ) {
        let (losses, gap, lag) = (self.losses, self.gap, self.lag);
        // Loss k is frame 1 + k * gap; its ask comes `lag` frames later, before that frame.
        let base = lag + 1;
        let last = base + losses * gap;
        let (mut lost, mut anchors, mut idrs) = (Vec::new(), Vec::new(), Vec::new());
        let mut aus: Vec<crate::EncodedFrame> = Vec::new();
        // Under acks the first loss waits a gap, until a confirmed frame can exist.
        let skip = usize::from(self.acked);
        let lost_at = |j: usize| {
            j >= 1 && (j - 1) % gap == 0 && (skip..losses + skip).contains(&((j - 1) / gap))
        };
        let last = last + skip * gap;
        for i in 0..=last {
            if self.acked {
                let last = i
                    .checked_sub(lag)
                    .and_then(|top| (0..=top).rev().find(|&j| !lost_at(j)));
                let acked = last.map(|last| crate::codec::Acked {
                    last: last as i64,
                    mask: (0..16)
                        .filter(|&k| last > k && !lost_at(last - 1 - k))
                        .fold(0u16, |m, k| m | 1 << k),
                });
                enc.set_reference_floor(acked);
                if lost_at(i) {
                    lost.push(i);
                }
            } else if i >= base && (i - base) % gap == 0 && (i - base) / gap < losses {
                let l = (i - lag) as i64;
                lost.push(i - lag);
                if enc.invalidate_ref_frames(l, l) {
                    anchors.push(i);
                } else {
                    enc.request_keyframe();
                    idrs.push(i);
                }
            }
            enc.submit_indexed(&frame(i), i as u32).expect("submit");
            while let Some(au) = enc.poll().expect("poll") {
                aus.push(au);
            }
        }
        enc.flush().expect("flush");
        while let Some(au) = enc.poll().expect("drain") {
            aus.push(au);
        }
        aus.sort_by_key(|a| a.pts_ns);
        assert_eq!(aus.len(), last + 1, "one AU per frame");
        for (i, au) in aus.iter().enumerate().filter(|_| !self.acked) {
            assert_eq!(
                au.recovery_anchor,
                anchors.contains(&i),
                "AU {i}: anchors where answered"
            );
            assert!(
                !idrs.contains(&i) || au.keyframe,
                "AU {i}: a declined ask is an IDR"
            );
        }
        if self.acked {
            anchors = (0..=last).filter(|&i| aus[i].recovery_anchor).collect();
        }
        let csv = |v: &[usize]| {
            v.iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(",")
        };
        let Soak {
            w, h, fps, mbps, ..
        } = self;
        println!(
            "{name}_ltr_anchor_soak: {w}x{h} {fps} fps {mbps} Mbps {:?} lag={lag} gap={gap} \
             acked={} lost={} anchors={} idrs={}",
            self.codec,
            self.acked,
            csv(&lost),
            csv(&anchors),
            csv(&idrs)
        );
        if self.acked {
            // The frame after each loss must lean on a confirmed one; the decode judges it.
            for &l in &lost {
                assert!(
                    aus[l + 1].recovery_anchor,
                    "AU {}: a confirmed reference",
                    l + 1
                );
            }
            assert!(aus[1..].iter().all(|a| !a.keyframe), "no IDR under acks");
        }
        if let Ok(dir) = std::env::var("PUNKTFUNK_SMOKE_DIR") {
            let ext = match self.codec {
                crate::Codec::H264 => "h264",
                crate::Codec::Av1 => "obu",
                _ => "h265",
            };
            let full: Vec<&[u8]> = aus.iter().map(|a| a.data.as_slice()).collect();
            let view: Vec<&[u8]> = aus
                .iter()
                .enumerate()
                .filter(|(i, _)| !lost.contains(i))
                .map(|(_, a)| a.data.as_slice())
                .collect();
            let mode = if self.acked { "acked" } else { "anchor" };
            write_capture(&format!("{dir}/{name}-{mode}.{ext}"), &full).expect("write");
            write_capture(&format!("{dir}/{name}-{mode}-dropS.{ext}"), &view).expect("write");
        }
    }
}

/// Fail unless the `impl` block at `marker` in `impl_src` writes every [`crate::Encoder`]
/// method. A wrapper that leaves one to the trait default silently disables it for every
/// session it holds. Source-text parse: an item ends at its first column-0 `}` and a method
/// name sits on a line starting `fn `. `find` takes the first occurrence, so the real item
/// must precede any quote of its marker.
#[cfg(any(test, feature = "test-support"))]
pub fn assert_writes_every_encoder_method(impl_src: &str, marker: &str) {
    fn item_block<'a>(src: &'a str, marker: &str) -> &'a str {
        let start = src
            .find(marker)
            .unwrap_or_else(|| panic!("marker {marker:?} not found — update this guard"));
        let body = &src[start..];
        let end = body
            .find("\n}")
            .unwrap_or_else(|| panic!("no column-0 close brace after {marker:?}"));
        &body[..end]
    }
    fn fn_names(block: &str) -> std::collections::BTreeSet<&str> {
        block
            .lines()
            .map(str::trim_start)
            .filter(|l| !l.starts_with("//"))
            .filter_map(|l| l.strip_prefix("fn "))
            .map(|rest| {
                rest.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                    .next()
                    .expect("split yields at least one item")
            })
            .collect()
    }
    let trait_fns = fn_names(item_block(
        include_str!("codec.rs"),
        "pub trait Encoder: Send {",
    ));
    let impl_fns = fn_names(item_block(impl_src, marker));
    assert!(
        trait_fns.len() >= 12,
        "only {} trait methods parsed — the extraction markers have rotted, fix the parse \
         before trusting this guard",
        trait_fns.len()
    );
    let missing: Vec<_> = trait_fns.difference(&impl_fns).collect();
    assert!(
        missing.is_empty(),
        "`{marker}` leaves Encoder methods to the trait default: {missing:?} — each one then \
         silently no-ops for every session this impl holds. Write each one out."
    );
    // An impl fn the trait lacks is a compile error; equality guards a parse regression.
    assert_eq!(trait_fns, impl_fns);
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_nv12_pattern_is_bt601_limited_range() {
        let nv12 = super::scroll_pattern_nv12(4, 2, 0);
        assert_eq!(nv12.len(), 12);
        assert_eq!(
            (nv12[0], nv12[8], nv12[9]),
            (82, 90, 240),
            "pure red at the origin"
        );
    }
}
