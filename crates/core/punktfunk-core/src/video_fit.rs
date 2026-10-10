//! Where a decoded frame lands in a view, and which filter scales it there.
//!
//! One rule for every client: [`place`] gives the visible frame region (`src`)
//! and the whole-pixel view rect it fills (`dst`). The presenter draws `src` into
//! `dst`; pointer, touch, pen and cursor map through the same [`Placement`].
//!
//! A scale within [`SNAP_PX`] / [`SNAP_REL`] of a whole number snaps to it: the
//! few pixels of difference become a bar or a crop, and the frame shows 1:1 or
//! pixel-replicated instead of resampled by 1.0005×.
//!
//! Twins: `PunktfunkShared/VideoFit.swift`, Android `VideoFit.kt`, client-web
//! `video-fit.ts`. All run `clients/shared/video-fit-vectors.json`.

/// How a frame whose aspect differs from the view fills it. Settings key `video_fit`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum VideoFit {
    /// Whole frame, bars.
    #[default]
    Fit,
    /// No bars; the overflow is cut off.
    Crop,
    /// No bars; each axis scaled on its own.
    Stretch,
}

impl VideoFit {
    /// Unknown names read as [`VideoFit::Fit`], so a newer client's value degrades safely.
    pub fn from_name(name: &str) -> VideoFit {
        match name {
            "crop" => VideoFit::Crop,
            "stretch" => VideoFit::Stretch,
            _ => VideoFit::Fit,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            VideoFit::Fit => "fit",
            VideoFit::Crop => "crop",
            VideoFit::Stretch => "stretch",
        }
    }

    /// The `Hello::video_fit` byte. `0` is Fit, so absence and older clients read as today.
    pub fn wire(self) -> u8 {
        match self {
            VideoFit::Fit => 0,
            VideoFit::Crop => 1,
            VideoFit::Stretch => 2,
        }
    }

    /// Unknown bytes read as [`VideoFit::Fit`].
    pub fn from_wire(b: u8) -> VideoFit {
        match b {
            1 => VideoFit::Crop,
            2 => VideoFit::Stretch,
            _ => VideoFit::Fit,
        }
    }
}

/// Resampling filter for one axis, picked from its scale by [`kernel`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kernel {
    /// Exactly 1:1.
    Copy,
    /// Whole-number upscale: pixel replication.
    Nearest,
    /// Fractional upscale.
    CatmullRom,
    /// Downscale, with the footprint widened by the ratio.
    Lanczos,
}

impl Kernel {
    pub fn name(self) -> &'static str {
        match self {
            Kernel::Copy => "copy",
            Kernel::Nearest => "nearest",
            Kernel::CatmullRom => "catmull-rom",
            Kernel::Lanczos => "lanczos",
        }
    }
}

/// Snap tolerance floor, in view pixels along the frame's longer axis.
pub const SNAP_PX: f64 = 8.0;
/// Snap tolerance as a fraction of the snapped scale.
pub const SNAP_REL: f64 = 0.005;

/// A frame region and the view rect it is drawn into. `dst` always lies inside the
/// view; `src` is the part of the frame that stays visible.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Placement {
    pub dst_x: u32,
    pub dst_y: u32,
    pub dst_w: u32,
    pub dst_h: u32,
    pub src_x: f64,
    pub src_y: f64,
    pub src_w: f64,
    pub src_h: f64,
    /// View pixels per frame pixel. Exact whole numbers when snapped.
    pub scale_x: f64,
    pub scale_y: f64,
}

impl Placement {
    const EMPTY: Placement = Placement {
        dst_x: 0,
        dst_y: 0,
        dst_w: 0,
        dst_h: 0,
        src_x: 0.0,
        src_y: 0.0,
        src_w: 0.0,
        src_h: 0.0,
        scale_x: 1.0,
        scale_y: 1.0,
    };

    /// Nothing to draw: a zero view or frame.
    pub fn is_empty(&self) -> bool {
        self.dst_w == 0 || self.dst_h == 0
    }

    pub fn kernel_x(&self) -> Kernel {
        kernel(self.scale_x)
    }

    pub fn kernel_y(&self) -> Kernel {
        kernel(self.scale_y)
    }

    /// View pixel → frame pixel, clamped onto the visible region.
    pub fn to_frame(&self, x: f64, y: f64) -> (f64, f64) {
        let fx = self.src_x + (x - f64::from(self.dst_x)) / self.scale_x;
        let fy = self.src_y + (y - f64::from(self.dst_y)) / self.scale_y;
        (
            fx.clamp(self.src_x, self.src_x + self.src_w),
            fy.clamp(self.src_y, self.src_y + self.src_h),
        )
    }

    /// Frame pixel → view pixel, clamped onto `dst`.
    pub fn to_view(&self, x: f64, y: f64) -> (f64, f64) {
        let x = x.clamp(self.src_x, self.src_x + self.src_w);
        let y = y.clamp(self.src_y, self.src_y + self.src_h);
        (
            f64::from(self.dst_x) + (x - self.src_x) * self.scale_x,
            f64::from(self.dst_y) + (y - self.src_y) * self.scale_y,
        )
    }
}

/// Filter for one axis scaled by `scale` view pixels per frame pixel.
pub fn kernel(scale: f64) -> Kernel {
    if scale == 1.0 {
        Kernel::Copy
    } else if scale > 1.0 && scale.fract() == 0.0 {
        Kernel::Nearest
    } else if scale > 1.0 {
        Kernel::CatmullRom
    } else {
        Kernel::Lanczos
    }
}

/// Place a `frame` (pixels) in a `view` (device pixels of the drawing surface).
pub fn place(fit: VideoFit, view: (u32, u32), frame: (u32, u32)) -> Placement {
    let (vw, vh) = view;
    let (fw, fh) = frame;
    if vw == 0 || vh == 0 || fw == 0 || fh == 0 {
        return Placement::EMPTY;
    }
    let sx = f64::from(vw) / f64::from(fw);
    let sy = f64::from(vh) / f64::from(fh);
    let long = f64::from(fw.max(fh));
    let (sx, sy) = match fit {
        VideoFit::Fit => {
            let s = snap(sx.min(sy), long);
            (s, s)
        }
        VideoFit::Crop => {
            let s = snap(sx.max(sy), long);
            (s, s)
        }
        VideoFit::Stretch => (snap(sx, f64::from(fw)), snap(sy, f64::from(fh))),
    };
    let (x0, w) = axis(vw, fw, sx);
    let (y0, h) = axis(vh, fh, sy);
    // Integer width over integer frame: exact whenever the snap made it whole.
    let scale_x = w as f64 / f64::from(fw);
    let scale_y = h as f64 / f64::from(fh);
    let (dst_x, dst_w) = clip(x0, w, vw);
    let (dst_y, dst_h) = clip(y0, h, vh);
    Placement {
        dst_x,
        dst_y,
        dst_w,
        dst_h,
        src_x: (f64::from(dst_x) - x0 as f64) / scale_x,
        src_y: (f64::from(dst_y) - y0 as f64) / scale_y,
        src_w: f64::from(dst_w) / scale_x,
        src_h: f64::from(dst_h) / scale_y,
        scale_x,
        scale_y,
    }
}

/// Round `s` to a whole scale when the difference is at most `SNAP_PX` along
/// `len` frame pixels or `SNAP_REL` of the scale, whichever is larger.
fn snap(s: f64, len: f64) -> f64 {
    let k = s.round();
    if k >= 1.0 && (k - s).abs() <= (SNAP_PX / len).max(SNAP_REL * k) {
        k
    } else {
        s
    }
}

/// Unclipped origin and size along one axis, centred with the origin floored.
fn axis(view: u32, frame: u32, scale: f64) -> (i64, i64) {
    let size = ((f64::from(frame) * scale).round() as i64).max(1);
    ((i64::from(view) - size).div_euclid(2), size)
}

fn clip(origin: i64, size: i64, view: u32) -> (u32, u32) {
    let start = origin.max(0);
    let end = (origin + size).min(i64::from(view));
    (start as u32, (end - start).max(0) as u32)
}

/// How a host encodes a picture sized for another screen (a join, a mirrored head) for one
/// client: the `crop` of the `source` it takes, scaled to `out`. Every size is even, so a
/// 4:2:0 chroma grid stays aligned. `Default` (all zero) maps nothing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Reframe {
    pub source: (u32, u32),
    /// `x, y, width, height` in source pixels.
    pub crop: [u32; 4],
    pub out: (u32, u32),
}

impl Reframe {
    /// The whole `source`, unscaled.
    pub fn full(source: (u32, u32)) -> Reframe {
        Reframe {
            source,
            crop: [0, 0, source.0, source.1],
            out: source,
        }
    }

    /// Frame `source` for a client whose `view` fills by `fit`. Crop takes the centred part of
    /// the view's shape; every fit then shrinks to fit inside the view and never grows. Stretch
    /// frames like Fit, since the client stretches whatever arrives.
    pub fn plan(fit: VideoFit, source: (u32, u32), view: (u32, u32)) -> Reframe {
        let (sw, sh) = (u64::from(source.0), u64::from(source.1));
        let (vw, vh) = (u64::from(view.0), u64::from(view.1));
        let mut r = Reframe::full(source);
        if sw == 0 || sh == 0 || vw == 0 || vh == 0 {
            return r;
        }
        if fit == VideoFit::Crop {
            // Within two pixels of the source's shape is the source: no crop for a rounding.
            let (cw, ch) = if sw * vh > sh * vw {
                ((sh * vw / vh) & !1, sh)
            } else {
                (sw, (sw * vh / vw) & !1)
            };
            if cw >= 2 && ch >= 2 && (sw - cw > 2 || sh - ch > 2) {
                r.crop = [
                    (((sw - cw) / 2) & !1) as u32,
                    (((sh - ch) / 2) & !1) as u32,
                    cw as u32,
                    ch as u32,
                ];
            }
        }
        r.out = crate::render_scale::fit_inside(r.crop[2], r.crop[3], view.0, view.1);
        r
    }

    /// Takes the whole source at its own size.
    pub fn is_full(&self) -> bool {
        *self == Reframe::full(self.source)
    }

    /// Cuts part of the source away.
    pub fn is_cropped(&self) -> bool {
        self.crop != [0, 0, self.source.0, self.source.1]
    }

    /// A point `x, y` in an `extent`-sized picture of the encoded frame, as source pixels.
    pub fn to_source(&self, x: f64, y: f64, extent: (f64, f64)) -> (f64, f64) {
        if extent.0 <= 0.0 || extent.1 <= 0.0 || self.source.0 == 0 {
            return (x, y);
        }
        let [cx, cy, cw, ch] = self.crop.map(f64::from);
        (cx + x / extent.0 * cw, cy + y / extent.1 * ch)
    }

    /// A source pixel as an encoded-frame pixel (the forwarded cursor). May fall outside.
    pub fn to_frame(&self, x: i32, y: i32) -> (i32, i32) {
        let [cx, cy, cw, ch] = self.crop.map(i64::from);
        if cw == 0 || ch == 0 {
            return (x, y);
        }
        let (ow, oh) = (i64::from(self.out.0), i64::from(self.out.1));
        (
            ((i64::from(x) - cx) * ow / cw) as i32,
            ((i64::from(y) - cy) * oh / ch) as i32,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EPS: f64 = 1e-6;

    fn dst(p: &Placement) -> [u32; 4] {
        [p.dst_x, p.dst_y, p.dst_w, p.dst_h]
    }

    #[test]
    fn fit_pillarboxes_a_16_9_frame_on_a_20_9_phone() {
        let p = place(VideoFit::Fit, (3216, 1440), (1920, 1080));
        assert_eq!(dst(&p), [328, 0, 2560, 1440]);
        assert_eq!(p.kernel_x(), Kernel::CatmullRom);
    }

    #[test]
    fn crop_cuts_the_top_and_bottom_instead() {
        let p = place(VideoFit::Crop, (3216, 1440), (1920, 1080));
        assert_eq!(dst(&p), [0, 0, 3216, 1440]);
        // 1809 rows centred in 1440 put the origin at floor(-184.5) = -185.
        assert!((p.src_y - 185.0 / 1.675).abs() < EPS, "{}", p.src_y);
        assert!((p.src_h - 859.701_492).abs() < EPS, "{}", p.src_h);
        assert_eq!((p.src_x, p.src_w), (0.0, 1920.0));
    }

    #[test]
    fn stretch_scales_each_axis_alone() {
        let p = place(VideoFit::Stretch, (3216, 1440), (1920, 1080));
        assert_eq!(dst(&p), [0, 0, 3216, 1440]);
        assert!((p.scale_x - 1.675).abs() < EPS);
        assert!((p.scale_y - 4.0 / 3.0).abs() < EPS);
    }

    #[test]
    fn an_odd_window_shows_the_frame_one_to_one() {
        let p = place(VideoFit::Fit, (1921, 1081), (1920, 1080));
        assert_eq!(dst(&p), [0, 0, 1920, 1080]);
        assert_eq!((p.kernel_x(), p.kernel_y()), (Kernel::Copy, Kernel::Copy));
    }

    #[test]
    fn a_near_double_window_replicates_pixels_and_crops_the_rest() {
        let p = place(VideoFit::Fit, (3832, 2152), (1920, 1080));
        assert_eq!(dst(&p), [0, 0, 3832, 2152]);
        assert_eq!((p.scale_x, p.scale_y), (2.0, 2.0));
        assert_eq!(
            (p.src_x, p.src_y, p.src_w, p.src_h),
            (2.0, 2.0, 1916.0, 1076.0)
        );
        assert_eq!(p.kernel_x(), Kernel::Nearest);
    }

    #[test]
    fn a_real_downscale_does_not_snap() {
        let p = place(VideoFit::Fit, (2732, 2048), (2880, 2160));
        assert_eq!(dst(&p), [0, 0, 2731, 2048]);
        assert_eq!(p.kernel_y(), Kernel::Lanczos);
    }

    #[test]
    fn mapping_round_trips_through_the_visible_region() {
        let p = place(VideoFit::Crop, (3216, 1440), (1920, 1080));
        let (fx, fy) = p.to_frame(1608.0, 720.0);
        assert!((fx - 960.0).abs() < EPS && (fy - (185.0 + 720.0) / 1.675).abs() < EPS);
        let (vx, vy) = p.to_view(fx, fy);
        assert!((vx - 1608.0).abs() < EPS && (vy - 720.0).abs() < EPS);
        // A frame point cropped away lands on the nearest visible edge.
        assert_eq!(p.to_view(0.0, 0.0), (0.0, 0.0));
        // A view point in a Fit bar lands on the frame edge.
        let fit = place(VideoFit::Fit, (3216, 1440), (1920, 1080));
        assert_eq!(fit.to_frame(0.0, 700.0).0, 0.0);
    }

    #[test]
    fn empty_inputs_place_nothing() {
        assert!(place(VideoFit::Fit, (0, 1080), (1920, 1080)).is_empty());
        assert!(place(VideoFit::Crop, (1920, 1080), (0, 0)).is_empty());
    }

    #[test]
    fn names_round_trip_and_unknown_is_fit() {
        for f in [VideoFit::Fit, VideoFit::Crop, VideoFit::Stretch] {
            assert_eq!(VideoFit::from_name(f.name()), f);
        }
        assert_eq!(VideoFit::from_name("zoom"), VideoFit::Fit);
        for f in [VideoFit::Fit, VideoFit::Crop, VideoFit::Stretch] {
            assert_eq!(VideoFit::from_wire(f.wire()), f);
        }
        assert_eq!(VideoFit::from_wire(9), VideoFit::Fit);
    }

    /// The cross-language contract; Swift, Kotlin and TypeScript run the same file.
    #[test]
    fn shared_vectors() {
        let raw = include_str!("../../../../clients/shared/video-fit-vectors.json");
        let file: serde_json::Value = serde_json::from_str(raw).expect("vector file parses");
        let cases = file["cases"].as_array().expect("cases array");
        assert!(
            cases.len() >= 12,
            "the vector file is the contract; keep it rich"
        );
        let num = |v: &serde_json::Value| v.as_f64().expect("number");
        for case in cases {
            let name = case["name"].as_str().unwrap();
            let pair = |k: &str| {
                let a = case[k].as_array().unwrap();
                (num(&a[0]) as u32, num(&a[1]) as u32)
            };
            let fit = VideoFit::from_name(case["fit"].as_str().unwrap());
            let p = place(fit, pair("view"), pair("frame"));
            let want = &case["expect"];
            let d: Vec<u32> = want["dst"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| num(v) as u32)
                .collect();
            assert_eq!(dst(&p).to_vec(), d, "{name} dst");
            let s: Vec<f64> = want["src"].as_array().unwrap().iter().map(num).collect();
            let got = [p.src_x, p.src_y, p.src_w, p.src_h];
            for (i, (g, w)) in got.iter().zip(&s).enumerate() {
                assert!((g - w).abs() < 1e-4, "{name} src[{i}] {g} != {w}");
            }
            let k = want["kernel"].as_array().unwrap();
            assert_eq!(
                p.kernel_x().name(),
                k[0].as_str().unwrap(),
                "{name} kernel x"
            );
            assert_eq!(
                p.kernel_y().name(),
                k[1].as_str().unwrap(),
                "{name} kernel y"
            );
            for m in want["to_frame"].as_array().into_iter().flatten() {
                let m: Vec<f64> = m.as_array().unwrap().iter().map(num).collect();
                let (fx, fy) = p.to_frame(m[0], m[1]);
                assert!(
                    (fx - m[2]).abs() < 1e-4 && (fy - m[3]).abs() < 1e-4,
                    "{name} to_frame {m:?} got {fx},{fy}"
                );
            }
        }
    }

    #[test]
    fn reframe_crops_a_4k_owner_to_a_20_9_joiner() {
        let r = Reframe::plan(VideoFit::Crop, (3840, 2160), (2400, 1080));
        assert_eq!(r.crop, [0, 216, 3840, 1728]);
        assert_eq!(r.out, (2400, 1080));
        assert!(r.is_cropped());
        // The joiner's centre is the owner's centre; its corners are the crop's.
        assert_eq!(
            r.to_source(1200.0, 540.0, (2400.0, 1080.0)),
            (1920.0, 1080.0)
        );
        assert_eq!(r.to_source(0.0, 0.0, (2400.0, 1080.0)), (0.0, 216.0));
        assert_eq!(r.to_frame(1920, 1080), (1200, 540));
        assert_eq!(r.to_frame(0, 216), (0, 0));
    }

    #[test]
    fn reframe_fit_only_shrinks() {
        let r = Reframe::plan(VideoFit::Fit, (3840, 2160), (2400, 1080));
        assert_eq!((r.crop, r.out), ([0, 0, 3840, 2160], (1920, 1080)));
        assert!(!r.is_cropped() && !r.is_full());
        assert_eq!(
            Reframe::plan(VideoFit::Stretch, (3840, 2160), (2400, 1080)),
            r
        );
        // A larger view keeps the source as it is.
        assert!(Reframe::plan(VideoFit::Fit, (1920, 1080), (3840, 2160)).is_full());
    }

    #[test]
    fn reframe_crop_keeps_a_matching_shape_and_even_origins() {
        // 1920x1200 into 16:9: 1080 rows, 60 cut from each edge.
        let r = Reframe::plan(VideoFit::Crop, (1920, 1200), (1280, 720));
        assert_eq!((r.crop, r.out), ([0, 60, 1920, 1080], (1280, 720)));
        // A shape within a rounding of the source is not cropped.
        assert!(!Reframe::plan(VideoFit::Crop, (1920, 1080), (1366, 768)).is_cropped());
        // An odd margin floors to an even origin.
        let r = Reframe::plan(VideoFit::Crop, (1920, 1200), (2560, 1080));
        assert_eq!(r.crop, [0, 194, 1920, 810]);
        assert!(Reframe::default().to_frame(5, 7) == (5, 7));
    }
}
