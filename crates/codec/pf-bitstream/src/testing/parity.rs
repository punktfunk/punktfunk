//! The libavcodec parity fixtures every native decode rung hashes against, and the CPU half
//! of those legs: golden sets, the planner's decode and display order, and the plane
//! localiser a mismatch report prints. Each rung keeps its GPU readback and hashing.
//!
//! Streams and goldens stay in `pf-vkdecode/tests/data`, included by path; each golden
//! file's header names the ffmpeg command that made it. The guards below run once, here,
//! for every rung: a re-synced fixture fails in CI, not as a frame-count mismatch on a GPU.

use std::collections::HashSet;

use crate::av1::Av1Planner;
use crate::h264::DisplayCrop;
use crate::h264::H264Planner;
use crate::h265;
use crate::h265::H265Planner;

/// The splitter and planner a fixture takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    H264,
    H265,
    Av1,
}

/// The packed 4:2:0 layout a golden hashes: luma rows, then interleaved chroma.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    Nv12,
    /// Two bytes per sample, ten bits in the high end of each little-endian word.
    P010,
}

impl Layout {
    pub fn bit_depth(self) -> u8 {
        match self {
            Layout::Nv12 => 8,
            Layout::P010 => 10,
        }
    }
}

/// One stream, its goldens, and what a leg asserts about it. The three counts stay
/// separate: the vendored AV1 vector is 250 units / 274 decoded / 250 shown and our own
/// streams show one frame per unit, so a count derived from another stops checking a shape.
#[derive(Debug, Clone, Copy)]
pub struct Fixture {
    pub label: &'static str,
    pub codec: Codec,
    pub layout: Layout,
    pub bytes: &'static [u8],
    /// One SHA-256 per display-order frame; `#` lines are the provenance header.
    pub golden_file: &'static str,
    pub golden_path: &'static str,
    /// Access units, or temporal units on AV1.
    pub units: usize,
    pub decoded: usize,
    pub shown: usize,
    /// The region the goldens hash, at (0, 0). The rungs refuse any other origin.
    pub display: (u32, u32),
}

/// The vendored H.264 vector. It reorders, so a rung that presents at decode time needs
/// [`Order`] to compare.
pub const H264: Fixture = Fixture {
    label: "H.264",
    codec: Codec::H264,
    layout: Layout::Nv12,
    bytes: super::H264_25FPS,
    golden_file: include_str!("../../../pf-vkdecode/tests/data/test-25fps.nv12.sha256"),
    golden_path: "pf-vkdecode/tests/data/test-25fps.nv12.sha256",
    units: 250,
    decoded: 250,
    shown: 250,
    display: (320, 240),
};

/// Host low-delay H.264. `max_num_ref_frames = 3` equals the DPB depth with no reorder, so
/// the sliding-window unmark and the eviction share an AU and a removed picture is still
/// named by that AU's references. The vendored vector never reaches that shape.
pub const H264_LOWDELAY: Fixture = Fixture {
    label: "H.264 (low-delay host stream)",
    codec: Codec::H264,
    layout: Layout::Nv12,
    bytes: include_bytes!("../../../pf-vkdecode/tests/data/lowdelay-640x480.h264"),
    golden_file: include_str!("../../../pf-vkdecode/tests/data/lowdelay-640x480.nv12.sha256"),
    golden_path: "pf-vkdecode/tests/data/lowdelay-640x480.nv12.sha256",
    units: 120,
    decoded: 120,
    shown: 120,
    display: (640, 480),
};

/// The vendored HEVC vector: reorders, one IRAP, no conformance window.
pub const H265: Fixture = Fixture {
    label: "H.265",
    codec: Codec::H265,
    layout: Layout::Nv12,
    bytes: super::H265_25FPS,
    golden_file: include_str!("../../../pf-vkdecode/tests/data/test-25fps-h265.nv12.sha256"),
    golden_path: "pf-vkdecode/tests/data/test-25fps-h265.nv12.sha256",
    units: 250,
    decoded: 250,
    shown: 250,
    display: (320, 240),
};

/// Host low-delay HEVC: a five-picture DPB, four marked, no reorder, so an RPS drop and its
/// eviction share an AU. The planner snapshots `dpb_refs` after `decode_rps`, so the
/// dropped picture is never a reference of that AU.
pub const H265_LOWDELAY: Fixture = Fixture {
    label: "H.265 (low-delay host stream)",
    codec: Codec::H265,
    layout: Layout::Nv12,
    bytes: include_bytes!("../../../pf-vkdecode/tests/data/lowdelay-640x480.h265"),
    golden_file: include_str!("../../../pf-vkdecode/tests/data/lowdelay-640x480-h265.nv12.sha256"),
    golden_path: "pf-vkdecode/tests/data/lowdelay-640x480-h265.nv12.sha256",
    units: 120,
    decoded: 120,
    shown: 120,
    display: (640, 480),
};

/// Ten-bit HEVC. P010 is also the layout of Vulkan's `3PACK16` pool, so one golden file
/// serves every rung.
pub const MAIN10: Fixture = Fixture {
    label: "HEVC Main 10",
    codec: Codec::H265,
    layout: Layout::P010,
    bytes: include_bytes!("../../../pf-vkdecode/tests/data/test-main10.h265"),
    golden_file: include_str!("../../../pf-vkdecode/tests/data/test-main10.p010.sha256"),
    golden_path: "pf-vkdecode/tests/data/test-main10.p010.sha256",
    units: 50,
    decoded: 50,
    shown: 50,
    display: (320, 240),
};

/// The vendored AV1 vector, IVF: 24 of its units carry a hidden frame (decoded, referenced,
/// never shown) beside the shown one.
pub const AV1: Fixture = Fixture {
    label: "AV1",
    codec: Codec::Av1,
    layout: Layout::Nv12,
    bytes: super::AV1_25FPS,
    golden_file: include_str!("../../../pf-vkdecode/tests/data/test-25fps-av1.nv12.sha256"),
    golden_path: "pf-vkdecode/tests/data/test-25fps-av1.nv12.sha256",
    units: 250,
    decoded: 274,
    shown: 250,
    display: (320, 240),
};

/// Host AV1 at 4K, the only resolution our encoder splits into two tile rows, both in one
/// Tile Group OBU. A file covers decode, not fragmentation or reassembly.
pub const AV1_LOWDELAY: Fixture = Fixture {
    label: "AV1 (low-delay host stream, 4K two-tile)",
    codec: Codec::Av1,
    layout: Layout::Nv12,
    bytes: include_bytes!("../../../pf-vkdecode/tests/data/lowdelay-3840x2160.ivf.av1"),
    golden_file: include_str!("../../../pf-vkdecode/tests/data/lowdelay-3840x2160-av1.nv12.sha256"),
    golden_path: "pf-vkdecode/tests/data/lowdelay-3840x2160-av1.nv12.sha256",
    units: 60,
    decoded: 60,
    shown: 60,
    display: (3840, 2160),
};

pub const ALL: [Fixture; 7] = [
    H264,
    H264_LOWDELAY,
    H265,
    H265_LOWDELAY,
    MAIN10,
    AV1,
    AV1_LOWDELAY,
];

/// Frame 0 of [`AV1`] as libavcodec decodes it: packed NV12, hashed by the golden set's
/// first line. Intra-only, so a mismatch is this picture and not a reference.
pub const AV1_FRAME0: &[u8] =
    include_bytes!("../../../pf-vkdecode/tests/data/test-25fps-av1.frame0.nv12");

impl Fixture {
    /// The stream cut into units: Annex-B access units, or IVF temporal units.
    pub fn split(&self) -> Vec<&'static [u8]> {
        match self.codec {
            Codec::H264 => super::split_h264_aus(self.bytes),
            Codec::H265 => super::split_h265_aus(self.bytes),
            Codec::Av1 => super::split_ivf(self.bytes),
        }
    }

    /// The golden set, refused unless [`assert_real_set`] holds.
    pub fn goldens(&self) -> Vec<&'static str> {
        let goldens = golden_hashes(self.golden_file);
        assert_real_set(&goldens, self.shown, self.golden_path);
        goldens
    }

    /// The planner walk over [`Fixture::split`]. Panics on a picture outside the display
    /// region or layout, and unless the stream opens on an IDR or key frame.
    pub fn order(&self) -> Order {
        let units = self.split();
        match self.codec {
            Codec::H264 => order_h264(self, &units),
            Codec::H265 => order_h265(self, &units),
            Codec::Av1 => order_av1(self, &units),
        }
    }
}

/// The digest lines of a golden file, header and blank lines dropped.
pub fn golden_hashes(file: &'static str) -> Vec<&'static str> {
    file.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .collect()
}

/// Refuse a golden set that would make a parity verdict vacuous: short (a wiped file agrees
/// with a rung that delivered nothing), not 64 hex digits, or with repeats (a rung that
/// froze on one frame would pass).
pub fn assert_real_set(goldens: &[&str], expected: usize, path: &str) {
    assert_eq!(
        goldens.len(),
        expected,
        "{path} must carry one hash per display frame"
    );
    assert!(
        goldens
            .iter()
            .all(|line| line.len() == 64 && line.bytes().all(|b| b.is_ascii_hexdigit())),
        "{path}: every golden line is a bare lowercase SHA-256 hex digest"
    );
    let distinct = goldens.iter().collect::<HashSet<_>>().len();
    assert_eq!(
        distinct,
        goldens.len(),
        "{path}: {distinct} of {} goldens are distinct — a set with repeats would let a \
         rung that froze on a single frame pass parity",
        goldens.len()
    );
}

/// A planner walk beside the rung, as `PicId`s. The planners are deterministic, so the ids
/// are the rung's own.
#[derive(Debug, Default)]
pub struct Order {
    /// One id per decoded picture, in submission order.
    pub decode: Vec<u64>,
    /// The same ids in output order, flush included.
    pub display: Vec<u64>,
    /// Ids each unit decodes: one on H.264/H.265, none for a skipped RASL picture, and one
    /// per frame on AV1, where a unit can carry several.
    pub per_unit: Vec<Vec<u64>>,
}

fn crop_of(c: &DisplayCrop) -> [u32; 4] {
    [c.x, c.y, c.width, c.height]
}

/// Every picture is the fixture's display region at (0, 0), 4:2:0 at its layout's depth.
fn assert_picture(f: &Fixture, unit: usize, crop: [u32; 4], format: [u8; 3]) {
    let (w, h) = f.display;
    assert_eq!(
        crop,
        [0, 0, w, h],
        "{} unit {unit}: the goldens hash the {w}x{h} region at (0, 0)",
        f.label
    );
    let depth = f.layout.bit_depth();
    assert_eq!(
        format,
        [1, depth, depth],
        "{} unit {unit}: 4:2:0 at {depth} bits, the profile the legs open and the layout \
         they hash",
        f.label
    );
}

fn order_h264(f: &Fixture, aus: &[&[u8]]) -> Order {
    let mut planner = H264Planner::new();
    let mut order = Order::default();
    for (index, au) in aus.iter().enumerate() {
        let plan = planner.plan_au(au).unwrap_or_else(|e| {
            panic!(
                "{} AU {index}: the clean stream must plan, got {e:?}",
                f.label
            )
        });
        let p = &plan.picture;
        let format = [
            p.chroma_format_idc,
            p.bit_depth_luma_minus8 + 8,
            p.bit_depth_chroma_minus8 + 8,
        ];
        assert_picture(f, index, crop_of(&p.display_crop), format);
        assert!(
            index > 0 || p.is_idr,
            "{}: the stream opens on an IDR",
            f.label
        );
        let id = plan
            .dpb
            .stored
            .unwrap_or_else(|| panic!("{} AU {index}: every picture is stored", f.label));
        order.decode.push(id);
        order.per_unit.push(vec![id]);
        order.display.extend(plan.dpb.outputs.iter().copied());
    }
    order.display.extend(planner.flush().outputs);
    order
}

/// A RASL picture the planner skips decodes nothing: its unit gets no id.
fn order_h265(f: &Fixture, aus: &[&[u8]]) -> Order {
    let mut planner = H265Planner::new();
    let mut order = Order::default();
    for (index, au) in aus.iter().enumerate() {
        let plan = match planner.plan_au(au) {
            Ok(plan) => plan,
            Err(h265::PlanError::RaslSkipped { .. }) => {
                order.per_unit.push(Vec::new());
                continue;
            }
            Err(e) => panic!(
                "{} AU {index}: the clean stream must plan, got {e:?}",
                f.label
            ),
        };
        let p = &plan.picture;
        let format = [
            p.chroma_format_idc,
            p.bit_depth_luma_minus8 + 8,
            p.bit_depth_chroma_minus8 + 8,
        ];
        assert_picture(f, index, crop_of(&p.display_crop), format);
        assert!(
            index > 0 || p.is_idr,
            "{}: the stream opens on an IDR",
            f.label
        );
        let id = plan
            .dpb
            .stored
            .unwrap_or_else(|| panic!("{} AU {index}: every picture is stored", f.label));
        order.decode.push(id);
        order.per_unit.push(vec![id]);
        order.display.extend(plan.dpb.outputs.iter().copied());
    }
    order.display.extend(planner.flush().outputs);
    order
}

/// One id per decoded frame. AV1 has no bumping, so no flush. The decoded picture is the
/// render region (no superres), and no fixture carries film grain: the legs probe that
/// profile, and a concealment warning would hash concealed pixels against clean goldens.
fn order_av1(f: &Fixture, units: &[&[u8]]) -> Order {
    let mut planner = Av1Planner::new();
    let mut order = Order::default();
    for (index, unit) in units.iter().enumerate() {
        let plans = planner.plan_au(unit).unwrap_or_else(|e| {
            panic!(
                "{} unit {index}: the clean stream must plan, got {e:?}",
                f.label
            )
        });
        let mut this_unit = Vec::new();
        for plan in &plans {
            let p = &plan.picture;
            assert!(
                plan.warnings.is_empty() && !plan.sequence.film_grain_params_present,
                "{} unit {index}: no concealment and no film grain, got {:?}",
                f.label,
                plan.warnings
            );
            let render = [0, 0, p.render_width, p.render_height];
            assert_picture(
                f,
                index,
                render,
                [p.chroma_format_idc, p.bit_depth, p.bit_depth],
            );
            assert_eq!(
                (p.upscaled_width, p.frame_height),
                f.display,
                "{} unit {index}: the decoded picture is the render region",
                f.label
            );
            assert!(
                !order.decode.is_empty() || p.is_key,
                "{}: the stream opens on a key frame",
                f.label
            );
            if let Some(id) = plan.dpb.stored {
                order.decode.push(id);
                this_unit.push(id);
            }
            order.display.extend(plan.dpb.outputs.iter().copied());
        }
        order.per_unit.push(this_unit);
    }
    order
}

/// Where `ours` differs from `want`, two frames of one [`Layout`]. A hash names no cause;
/// these fields do:
///
/// - luma clean, chroma not: the chroma plane's copy region or origin.
/// - a luma shift fits better than in place: crop origin or copy extent, not decode.
/// - deltas of a few codes: an in-loop filter (deblock, CDEF, restoration).
/// - large structured deltas: quantisation or tile payloads.
/// - a constant plane: nothing was decoded into it.
/// - P010 samples with their low bits set: the samples are LSB-aligned, a format error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Divergence {
    pub luma_samples: usize,
    pub chroma_samples: usize,
    /// `(x0, y0, x1, y1)` around every differing luma sample.
    pub luma_box: Option<(u32, u32, u32, u32)>,
    /// In code values: P010 samples count their ten bits.
    pub max_delta: u32,
    /// |delta| in buckets 1, 2, 3-4, 5-8, 9-16, 17-64, 65+.
    pub histogram: [usize; 7],
    pub low_bits_set: usize,
    /// `ours` holds one value across its whole (luma, chroma) plane.
    pub flat: (bool, bool),
    /// `(dy, dx)` that `ours` matches `want` displaced by, clearly better than in place.
    pub shift: Option<(i32, i32)>,
}

impl std::fmt::Display for Divergence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.luma_samples == 0 && self.chroma_samples == 0 {
            return write!(f, "identical");
        }
        let [d1, d2, d4, d8, d16, d64, rest] = self.histogram;
        write!(
            f,
            "{} luma sample(s), {} chroma sample(s), max |delta| {} \
             (1:{d1} 2:{d2} 3-4:{d4} 5-8:{d8} 9-16:{d16} 17-64:{d64} 65+:{rest})",
            self.luma_samples, self.chroma_samples, self.max_delta
        )?;
        if let Some((x0, y0, x1, y1)) = self.luma_box {
            write!(
                f,
                ", luma bounding box ({x0},{y0})..({x1},{y1}) = {}x{}",
                x1 - x0 + 1,
                y1 - y0 + 1
            )?;
        }
        if self.chroma_samples == 0 {
            write!(f, ", chroma CLEAN")?;
        }
        for (flat, plane) in [(self.flat.0, "luma"), (self.flat.1, "chroma")] {
            if flat {
                write!(
                    f,
                    ", our {plane} is one value — nothing was decoded into it"
                )?;
            }
        }
        if let Some((dy, dx)) = self.shift {
            write!(
                f,
                ", a luma shift of dy{dy:+} dx{dx:+} fits better than in place — readback \
                 geometry, not decode"
            )?;
        }
        if self.low_bits_set > 0 {
            write!(
                f,
                ", and {} sample(s) have their low six bits set — P010's ten bits belong in \
                 the HIGH end of each word, so suspect the FORMAT before the decode",
                self.low_bits_set
            )?;
        }
        Ok(())
    }
}

impl Divergence {
    /// Count one differing sample `i` of `delta` code values in its plane, its bucket, and
    /// for luma the bounding box.
    fn record(&mut self, i: usize, delta: u32, width: usize, luma: usize) {
        self.max_delta = self.max_delta.max(delta);
        let bucket = [1, 2, 4, 8, 16, 64]
            .iter()
            .take_while(|&&top| delta > top)
            .count();
        self.histogram[bucket] += 1;
        if i >= luma {
            self.chroma_samples += 1;
            return;
        }
        self.luma_samples += 1;
        let (x, y) = ((i % width) as u32, (i / width) as u32);
        self.luma_box = Some(match self.luma_box {
            None => (x, y, x, y),
            Some((x0, y0, x1, y1)) => (x0.min(x), y0.min(y), x1.max(x), y1.max(y)),
        });
    }
}

/// Compare two packed frames of `layout` at `display`. See [`Divergence`].
pub fn localise(ours: &[u8], want: &[u8], display: (u32, u32), layout: Layout) -> Divergence {
    let p010 = layout == Layout::P010;
    let sample = |buf: &[u8], i: usize| -> u32 {
        if p010 {
            u32::from(u16::from_le_bytes([buf[2 * i], buf[2 * i + 1]]))
        } else {
            u32::from(buf[i])
        }
    };
    let code = |raw: u32| if p010 { raw >> 6 } else { raw };
    let (width, height) = (display.0 as usize, display.1 as usize);
    let luma = width * height;
    let samples = ours.len().min(want.len()) / if p010 { 2 } else { 1 };
    let flat = |range: std::ops::Range<usize>| {
        range
            .clone()
            .all(|i| sample(ours, i) == sample(ours, range.start))
    };
    let mut d = Divergence {
        luma_samples: 0,
        chroma_samples: 0,
        luma_box: None,
        max_delta: 0,
        histogram: [0; 7],
        low_bits_set: 0,
        flat: (flat(0..luma.min(samples)), flat(luma.min(samples)..samples)),
        shift: None,
    };
    for i in 0..samples {
        let (a, b) = (sample(ours, i), sample(want, i));
        d.low_bits_set += usize::from(p010 && a & 0x3f != 0);
        if a != b {
            d.record(i, code(a).abs_diff(code(b)), width, luma);
        }
    }
    if d.luma_samples > 0 && samples >= luma {
        d.shift = luma_shift(|i| sample(ours, i), |i| sample(want, i), width, height);
    }
    d
}

/// The luma displacement that matches clearly better than in place: a wrong crop origin or a
/// copy extent taken from the pool. Probes a central window of at most 256×256, 8 samples in
/// from every edge, so a 4K frame stays cheap.
fn luma_shift(
    ours: impl Fn(usize) -> u32,
    want: impl Fn(usize) -> u32,
    width: usize,
    height: usize,
) -> Option<(i32, i32)> {
    let window =
        |n: usize| (n / 2).saturating_sub(128).max(8)..(n / 2 + 128).min(n.saturating_sub(8));
    let (rows, cols) = (window(height), window(width));
    if rows.is_empty() || cols.is_empty() {
        return None;
    }
    let score = |dy: i32, dx: i32| {
        let hits: usize = rows
            .clone()
            .flat_map(|y| cols.clone().map(move |x| (y, x)))
            .filter(|&(y, x)| {
                let (sy, sx) = (
                    y.wrapping_add_signed(dy as isize),
                    x.wrapping_add_signed(dx as isize),
                );
                ours(y * width + x) == want(sy * width + sx)
            })
            .count();
        hits as f64 / (rows.len() * cols.len()) as f64
    };
    let in_place = score(0, 0);
    let (dy, dx, best) = (-4..=4)
        .flat_map(|dy| (-8..=8).map(move |dx| (dy, dx)))
        .filter(|&shift| shift != (0, 0))
        .map(|(dy, dx)| (dy, dx, score(dy, dx)))
        .fold((0, 0, f64::MIN), |a, b| if b.2 > a.2 { b } else { a });
    (best > in_place + 0.05).then_some((dy, dx))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_fixture_agrees_with_its_goldens_and_its_planner() {
        for f in ALL {
            let goldens = f.goldens();
            let units = f.split();
            assert_eq!(
                units.len(),
                f.units,
                "{}: the splitter's unit count",
                f.label
            );
            assert!(
                units.iter().all(|u| !u.is_empty()),
                "{}: no empty unit — a leg would decode nothing and blame the rung",
                f.label
            );
            let order = f.order();
            assert_eq!(
                (
                    order.decode.len(),
                    order.per_unit.len(),
                    order.display.len()
                ),
                (f.decoded, f.units, goldens.len()),
                "{}: decoded pictures, units, and displayed pictures against the goldens",
                f.label
            );
            let decoded: HashSet<_> = order.decode.iter().collect();
            assert!(
                order.display.iter().all(|id| decoded.contains(id)),
                "{}: display order names a picture nothing decodes",
                f.label
            );
        }
    }

    /// The legs reorder by `PicId` because the vendored vectors do. Our host emits zero
    /// reorder; if that stops, the low-delay fixtures no longer stand for what we stream.
    #[test]
    fn the_vendored_vectors_reorder_and_our_own_streams_do_not() {
        for f in [H264, H265] {
            let order = f.order();
            assert_ne!(
                order.decode, order.display,
                "{} no longer reorders",
                f.label
            );
        }
        for f in [H264_LOWDELAY, H265_LOWDELAY] {
            let order = f.order();
            assert_eq!(order.decode, order.display, "{} reorders", f.label);
        }
    }

    /// One IRAP, the opening IDR: a CRA/BLA would make RASL skips reachable and the frame
    /// counts need rederiving.
    #[test]
    fn the_h265_streams_hold_one_irap() {
        for f in [H265, H265_LOWDELAY] {
            let mut planner = H265Planner::new();
            let iraps: usize = f
                .split()
                .iter()
                .map(|au| usize::from(planner.plan_au(au).expect("plans").picture.is_irap))
                .sum();
            assert_eq!(iraps, 1, "{}: exactly one IRAP", f.label);
        }
    }

    /// Frame accounting, pinned and never derived: the vendored vector hides 24 frames in
    /// two-frame units, our host shows one frame per unit. Neither uses
    /// `show_existing_frame`, a display route the rungs handle apart.
    #[test]
    fn the_two_av1_streams_are_the_opposite_shapes_the_legs_claim() {
        for (f, want) in [
            (AV1, [274, 250, 24, 24, 0, 1]),
            (AV1_LOWDELAY, [60, 60, 0, 0, 0, 1]),
        ] {
            let mut planner = Av1Planner::new();
            let mut got = [0usize; 6];
            for unit in f.split() {
                let plans = planner.plan_au(unit).expect("the clean stream plans");
                got[2] += usize::from(plans.len() > 1);
                for plan in &plans {
                    got[0] += 1;
                    got[1] += plan.dpb.outputs.len();
                    got[3] += usize::from(!plan.picture.show_frame);
                    got[4] += usize::from(plan.dpb.stored.is_none());
                    got[5] += usize::from(plan.picture.is_key);
                }
            }
            assert_eq!(
                got, want,
                "{}: coded / displayed / multi-frame units / hidden / show_existing / key",
                f.label
            );
        }
    }

    /// The low-delay H.264 stream removes, on 117 AUs, a picture its own references still
    /// name: the aliasing every rung's release deferral exists for. Its SPS is why: a DPB
    /// exactly as deep as the three references, with no reorder.
    #[test]
    fn the_low_delay_h264_stream_still_removes_a_named_reference() {
        let mut planner = H264Planner::new();
        let mut both = 0usize;
        let mut first_sps = None;
        for au in H264_LOWDELAY.split() {
            let plan = planner.plan_au(au).expect("the low-delay stream plans");
            both += plan
                .dpb
                .removed
                .iter()
                .filter(|id| plan.dpb_refs.iter().any(|r| r.id == **id))
                .count();
            first_sps.get_or_insert((
                plan.sps.max_num_ref_frames,
                plan.picture.max_dpb_frames,
                plan.sps.vui_parameters.max_num_reorder_frames,
            ));
        }
        assert_eq!(
            first_sps,
            Some((3, 3, 0)),
            "max_num_ref_frames, DPB depth and max_num_reorder_frames"
        );
        assert_eq!(
            both, 117,
            "the stream must still remove pictures its own reference lists name — without \
             that its legs duplicate the conformance vector's"
        );
    }

    /// HEVC low-delay: `both == 0` alone is vacuous, so three numbers. 115 AUs remove a
    /// picture; none of them is in that AU's `dpb_refs` (the exemption); and 115 would be,
    /// against the set `decode_rps` sees. That set is exact: between AU N-1's snapshot and
    /// AU N's `decode_rps` only `finish_picture(N-1)` marks, so it is
    /// `dpb_refs(N-1) ∪ {stored(N-1)}`.
    #[test]
    fn the_low_delay_h265_stream_keeps_the_exemption_falsifiable() {
        let mut planner = H265Planner::new();
        let (mut with_removals, mut both, mut would_alias) = (0usize, 0usize, 0usize);
        let mut first_sps = None;
        let mut pre_rps_marked: Vec<u64> = Vec::new();
        for (index, au) in H265_LOWDELAY.split().iter().enumerate() {
            let plan = planner.plan_au(au).expect("the low-delay stream plans");
            with_removals += usize::from(!plan.dpb.removed.is_empty());
            both += plan
                .dpb
                .removed
                .iter()
                .filter(|id| plan.dpb_refs.iter().any(|r| r.id == **id))
                .count();
            would_alias += plan
                .dpb
                .removed
                .iter()
                .filter(|id| pre_rps_marked.contains(id))
                .count();
            first_sps.get_or_insert((
                plan.picture.max_dpb_frames,
                plan.sps.max_num_reorder_pics[usize::from(plan.sps.max_sub_layers_minus1)],
            ));
            pre_rps_marked = plan.dpb_refs.iter().map(|r| r.id).collect();
            if let Some(id) = plan.dpb.stored {
                assert!(
                    plan.picture.is_reference,
                    "AU {index}: a non-reference picture breaks the pre-RPS reconstruction"
                );
                pre_rps_marked.push(id);
            }
        }
        assert_eq!(
            first_sps,
            Some((5, 0)),
            "DPB depth and sps_max_num_reorder_pics"
        );
        assert_eq!(
            with_removals, 115,
            "a retirement on nearly every AU; else the two below are trivially zero"
        );
        assert_eq!(
            both, 0,
            "{both} picture(s) are in an AU's own marked DPB AND removed by it. HEVC must be \
             incapable of that: `H265Planner`'s snapshot moved ahead of `decode_rps`. Restore \
             the order, or give the HEVC conversions the release deferral; do NOT relax this"
        );
        assert_eq!(
            would_alias, 115,
            "the stream must stay CAPABLE of the defect it rules out; a reordering or deeper \
             regen reports 0 here and makes the zero above prove nothing"
        );
    }

    /// Two tile rows in one Tile Group OBU on every frame: the reason the 4K fixture exists.
    /// 55 of its 60 frames displace a reference they still name.
    #[test]
    fn the_low_delay_av1_stream_still_carries_two_tiles() {
        let mut planner = Av1Planner::new();
        let (mut with_removals, mut aliasing) = (0usize, 0usize);
        for (index, unit) in AV1_LOWDELAY.split().iter().enumerate() {
            for plan in planner.plan_au(unit).expect("the low-delay stream plans") {
                let tile = &plan.header.tile_info;
                assert_eq!(
                    (tile.tile_cols, tile.tile_rows),
                    (1, 2),
                    "unit {index}: a single-tile frame means a regen below 4K or an encoder \
                     that stopped splitting; regenerate at 3840x2160, do NOT relax this"
                );
                assert_eq!(
                    (
                        tile.width_in_sbs_minus_1[0],
                        tile.height_in_sbs_minus_1[0],
                        tile.height_in_sbs_minus_1[1],
                    ),
                    (59, 16, 16),
                    "unit {index}: the per-tile superblock sizing"
                );
                assert_eq!(
                    plan.tiles
                        .iter()
                        .map(|t| (t.tg_start, t.tg_end))
                        .collect::<Vec<_>>(),
                    [(0, 1)],
                    "unit {index}: one tile group covering tiles 0..=1 — 0..=0 is the \
                     truncation shape the host once shipped"
                );
                with_removals += usize::from(!plan.dpb.removed.is_empty());
                aliasing += plan
                    .dpb
                    .removed
                    .iter()
                    .filter(|id| plan.dpb_refs.iter().any(|r| r.id == **id))
                    .count();
            }
        }
        assert_eq!(
            (with_removals, aliasing),
            (55, 55),
            "a regen must keep the precondition `release_after_decode` exists for"
        );
    }

    #[test]
    fn a_divergence_names_the_plane_the_box_and_the_magnitude() {
        let (w, h) = (320u32, 240u32);
        let clean = vec![0x40u8; (w * h + w * h / 2) as usize];

        let mut one_block = clean.clone();
        for y in 24..48u32 {
            for x in 16..32u32 {
                one_block[(y * w + x) as usize] = 0x48;
            }
        }
        let d = localise(&one_block, &clean, (w, h), Layout::Nv12);
        assert_eq!(d.luma_samples, 16 * 24);
        assert_eq!(d.chroma_samples, 0);
        assert_eq!(d.luma_box, Some((16, 24, 31, 47)));
        assert_eq!(d.max_delta, 8);
        assert_eq!(d.histogram[3], 16 * 24, "every delta is 8");
        assert_eq!((d.flat, d.shift), ((false, true), None));
        assert!(format!("{d}").contains("chroma CLEAN"));
        assert!(format!("{d}").contains("16x24"));

        let structural = vec![0xffu8; clean.len()];
        let d = localise(&structural, &clean, (w, h), Layout::Nv12);
        assert_eq!(d.luma_samples, (w * h) as usize);
        assert_eq!(d.chroma_samples, (w * h / 2) as usize);
        assert_eq!(d.max_delta, 0xff - 0x40);
        assert!(!format!("{d}").contains("chroma CLEAN"));
        assert!(format!("{d}").contains("our luma is one value"));

        let d = localise(&clean, &clean, (w, h), Layout::Nv12);
        assert_eq!(d.luma_samples, 0);
        assert_eq!(format!("{d}"), "identical");
    }

    /// A picture read one row and two columns off: the localiser names the shift.
    #[test]
    fn a_displaced_picture_is_called_out_as_readback_geometry() {
        let (w, h) = (320usize, 240usize);
        let mut state = 0x2545_f491u32;
        let want: Vec<u8> = (0..w * h * 3 / 2)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                state as u8
            })
            .collect();
        let mut ours = want.clone();
        for y in 0..h - 1 {
            for x in 0..w - 2 {
                ours[y * w + x] = want[(y + 1) * w + x + 2];
            }
        }
        let d = localise(&ours, &want, (w as u32, h as u32), Layout::Nv12);
        assert_eq!(d.shift, Some((1, 2)));
        assert!(format!("{d}").contains("readback geometry"), "{d}");
    }

    /// P010 ten bits are high-aligned; LSB-aligned `yuv420p10le` has the right length.
    #[test]
    fn lsb_aligned_ten_bit_samples_are_called_out_as_a_format_problem() {
        let (w, h) = (16u32, 16u32);
        let samples = (w * h + w * h / 2) as usize;
        let msb: Vec<u8> = (0..samples).flat_map(|_| 0x0200u16.to_le_bytes()).collect();
        let lsb: Vec<u8> = (0..samples).flat_map(|_| 0x0008u16.to_le_bytes()).collect();

        let d = localise(&lsb, &msb, (w, h), Layout::P010);
        assert_eq!(d.low_bits_set, samples, "every sample carries low bits");
        assert!(
            format!("{d}").contains("low six bits"),
            "the report must point at the FORMAT: {d}"
        );

        let d = localise(&msb, &msb, (w, h), Layout::P010);
        assert_eq!(d.low_bits_set, 0);
        assert_eq!(format!("{d}"), "identical");
    }
}
