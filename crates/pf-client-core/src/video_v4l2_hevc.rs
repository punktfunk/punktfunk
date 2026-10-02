//! V4L2 stateless HEVC: the decoders that take parsed slices — the Raspberry
//! Pi 5's, Rockchip's. HEVC is the one codec the Pi decodes in hardware, and
//! the one with no CPU rung to fall onto.
//!
//! [`pf_v4l2dec::stateless::HevcDecoder`] turns each planned access unit into
//! one media request and keeps the decoded picture buffer; this module plans,
//! opens the node for the stream's shape, and hands pictures over. How they
//! leave depends on the format the driver decodes into:
//!
//! * linear NV12 — exported as a dma-buf and imported by the presenter, like
//!   the stateful rung's pictures;
//! * the Pi's column format — copied out into a [`CpuPlanarFrame`]. No Vulkan
//!   driver imports that layout, so the copy is what reaching the screen costs.
//!
//! 8-bit only: the 10-bit formats these drivers offer are packed layouts
//! nothing here can show. A picture whose reference was lost is withheld and
//! asks for a keyframe; it is not a decoder fault.

use std::os::fd::AsRawFd as _;
use std::os::fd::OwnedFd;
use std::path::Path;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::Arc;

use anyhow::anyhow;
use anyhow::bail;
use anyhow::Context as _;
use anyhow::Result;
use pf_bitstream::h265::AuPlan;
use pf_bitstream::h265::H265Planner;
use pf_bitstream::h265::PlanError;
use pf_bitstream::h265::PlanWarning;
use pf_v4l2::RequestNode;
use pf_v4l2dec::hevc::FillError;
use pf_v4l2dec::sand;
use pf_v4l2dec::stateful::fourcc_name;
use pf_v4l2dec::stateless;
use pf_v4l2dec::stateless::HevcDecoder;
use pf_v4l2dec::stateless::Shown;
use pf_v4l2dec::uapi;
use pf_v4l2dec::uapi_stateless;

use crate::video::CpuPlanarFrame;
use crate::video::DecodeHealth;
use crate::video::DecodedImage;
use crate::video::DmabufFrame;
use crate::video::DmabufPlane;
use crate::video::DrmFrameGuard;
use crate::video::FrameGuard;
use crate::video_v4l2::colour_of;
use crate::video_v4l2::display_of;
use crate::video_v4l2::Facts;
use crate::video_v4l2::Release;
use crate::video_v4l2::V4l2FrameGuard;
use crate::video_v4l2::DECODER_PIN;
use crate::video_v4l2::DRM_FORMAT_MOD_LINEAR;

/// What the rung needs of a stateless node beyond the request flow.
pub(crate) trait OpenedStateless: stateless::Device + Sized {
    fn open(path: &Path) -> std::io::Result<Self>;
    /// Export CAPTURE buffer `index` as a dma-buf.
    fn export(&mut self, index: u32) -> std::io::Result<OwnedFd>;
    /// The bytes of a CAPTURE buffer the decoder has returned.
    fn picture(&self, index: u32) -> Option<&[u8]>;
}

impl OpenedStateless for RequestNode {
    fn open(path: &Path) -> std::io::Result<RequestNode> {
        RequestNode::open(path)
    }

    fn export(&mut self, index: u32) -> std::io::Result<OwnedFd> {
        RequestNode::export(self, index)
    }

    fn picture(&self, index: u32) -> Option<&[u8]> {
        RequestNode::picture(self, index)
    }
}

/// What sizes the node's buffers. A change reopens it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Shape {
    coded: (u32, u32),
    bit_depth: u8,
    dpb: usize,
}

struct Session<D: OpenedStateless> {
    decoder: HevcDecoder<D, Facts>,
    shape: Shape,
    /// Names this session's buffers in release tokens and pool keys.
    pool: u32,
    /// dma-bufs of the picture buffers, exported on first use.
    exports: Option<Arc<Vec<OwnedFd>>>,
}

pub(crate) struct StatelessHevc<D: OpenedStateless = RequestNode> {
    node: PathBuf,
    planner: Box<H265Planner>,
    session: Option<Session<D>>,
    health: DecodeHealth,
    recovery_request: bool,
    release_tx: mpsc::Sender<Release>,
    release_rx: mpsc::Receiver<Release>,
}

impl<D: OpenedStateless> StatelessHevc<D> {
    /// The rung over the decoder at `node`, opened on the first keyframe.
    pub(crate) fn on_node(node: PathBuf) -> StatelessHevc<D> {
        let (release_tx, release_rx) = mpsc::channel();
        StatelessHevc {
            node,
            planner: Box::new(H265Planner::new()),
            session: None,
            health: DecodeHealth {
                // The driver has no per-picture status query this rung reads.
                status_queries: false,
                ..DecodeHealth::default()
            },
            recovery_request: false,
            release_tx,
            release_rx,
        }
    }

    pub(crate) fn health(&self) -> DecodeHealth {
        self.health
    }

    pub(crate) fn take_recovery_request(&mut self) -> bool {
        std::mem::take(&mut self.recovery_request)
    }

    pub(crate) fn forgive_unclean(&mut self) {
        self.planner.forgive_unclean();
    }

    /// One access unit in, at most one picture out. `Ok(None)` is a withheld
    /// picture, an HEVC RASL skip, or the wait for a keyframe.
    pub(crate) fn decode(&mut self, au: &[u8]) -> Result<Option<DecodedImage>> {
        self.drain_releases();
        let result = self.decode_inner(au);
        match &result {
            Ok(Some((_, damaged))) => self.health.note(*damaged, false, 0),
            Ok(None) => {}
            Err(_) => self.health.note(false, true, 0),
        }
        Ok(result?.and_then(|(image, _)| image))
    }

    fn decode_inner(&mut self, au: &[u8]) -> Result<Option<(Option<DecodedImage>, bool)>> {
        let plan = match self.planner.plan_au(au) {
            Ok(plan) => plan,
            // Spec 8.1.3 NOTE: a skipped RASL is not a loss.
            Err(PlanError::RaslSkipped { .. }) => return Ok(None),
            Err(e) if e.awaits_idr() => {
                self.recovery_request = true;
                return Ok(None);
            }
            Err(e) => return Err(anyhow!("{e:?}")),
        };
        let pic = &plan.picture;
        let facts = Facts {
            keyframe: pic.is_idr,
            references_clean: pic.references_clean,
            color: colour_of(&pic.colour),
            display: display_of(pic.display_crop)?,
            damaged: plan.warnings.iter().any(PlanWarning::is_integrity),
        };
        if facts.damaged {
            self.recovery_request = true;
        }
        // No session (the last request failed): only a keyframe starts one.
        if self.session.is_none() && !pic.is_irap {
            self.recovery_request = true;
            return Ok(None);
        }
        self.ensure_session(&plan, au)?;
        let s = self.session.as_mut().expect("just ensured");
        let shown = match s.decoder.decode(&plan, au, facts) {
            Ok(shown) => shown,
            // A picture this one predicts from never decoded. That is loss,
            // and it ends at the next keyframe.
            Err(stateless::Error::Fill(FillError::UnresolvedReference(_))) => {
                self.recovery_request = true;
                return Ok(Some((None, true)));
            }
            // A failed request keeps its OUTPUT buffer bound, so every later picture would
            // fail on it: drop the session and reopen at the next keyframe.
            Err(e) => {
                self.session = None;
                self.recovery_request = true;
                return Err(anyhow!("{e}"));
            }
        };
        // Hosts send one picture per unit; a bump of several keeps the newest.
        let mut newest: Option<Shown<Facts>> = None;
        for picture in shown {
            if let Some(older) = newest.replace(picture) {
                s.decoder.release(older.buffer);
                if self.health.note_dropped() {
                    tracing::warn!(
                        dropped_total = self.health.dropped,
                        "native V4L2: a reordering stream — showing the newest picture only"
                    );
                }
            }
        }
        let Some(picture) = newest else {
            return Ok(Some((None, facts.damaged)));
        };
        if picture.facts.damaged || picture.corrupt {
            self.recovery_request = true;
            s.decoder.release(picture.buffer);
            return Ok(Some((None, true)));
        }
        let image = self.ship(picture)?;
        Ok(Some((Some(image), facts.damaged)))
    }

    /// Open the node for this stream's shape. It can only start where the
    /// stream does: on a picture with no references.
    fn ensure_session(&mut self, plan: &AuPlan, au: &[u8]) -> Result<()> {
        let pic = &plan.picture;
        let shape = Shape {
            coded: (pic.coded_width, pic.coded_height),
            bit_depth: 8 + pic.bit_depth_luma_minus8,
            dpb: pic.max_dpb_frames,
        };
        if self.session.as_ref().is_some_and(|s| s.shape == shape) {
            return Ok(());
        }
        if !pic.is_irap {
            bail!("the stream changed shape outside a keyframe");
        }
        if let Some(old) = self.session.take() {
            tracing::info!(from = ?old.shape, to = ?shape,
                "V4L2 stream renegotiated — reopening the decoder");
        }
        let wanted: &[u32] = if shape.bit_depth > 8 {
            &[uapi::V4L2_PIX_FMT_P010]
        } else {
            &[
                uapi::V4L2_PIX_FMT_NV12,
                uapi_stateless::V4L2_PIX_FMT_NV12_COL128,
            ]
        };
        let node = D::open(&self.node).with_context(|| format!("open {}", self.node.display()))?;
        let decoder = HevcDecoder::open(node, plan, au, wanted)
            .map_err(|e| anyhow!("{e}"))
            .context("start the V4L2 stateless decoder")?;
        let format = decoder.format();
        tracing::info!(
            format = %fourcc_name(format.fourcc),
            coded = format_args!("{}x{}", format.width, format.height),
            buffers = decoder.buffers(),
            "native V4L2 stateless picture pool ready"
        );
        self.session = Some(Session {
            decoder,
            shape,
            pool: crate::video::next_pool_generation(),
            exports: None,
        });
        Ok(())
    }

    fn drain_releases(&mut self) {
        while let Ok(r) = self.release_rx.try_recv() {
            if let Some(s) = self.session.as_mut().filter(|s| s.pool == r.pool) {
                s.decoder.release(r.index);
            }
        }
    }

    fn ship(&mut self, picture: Shown<Facts>) -> Result<DecodedImage> {
        let s = self.session.as_mut().expect("decode built the session");
        let format = s.decoder.format();
        let facts = picture.facts;
        if format.fourcc == uapi_stateless::V4L2_PIX_FMT_NV12_COL128 {
            let bytes = s
                .decoder
                .device()
                .picture(picture.buffer)
                .ok_or_else(|| anyhow!("picture buffer {} is not mapped", picture.buffer))?;
            // For the column format `stride` is the column height in rows.
            let planar = sand::unpack(
                bytes,
                facts.display.0 as usize,
                facts.display.1 as usize,
                format.height as usize,
                format.stride as usize,
            )
            .map_err(|e| anyhow!("{e}"))?;
            // Copied out: the buffer is only a reference now.
            s.decoder.release(picture.buffer);
            let frame = CpuPlanarFrame::from_planes(
                facts.display.0,
                facts.display.1,
                [planar.y, planar.cb, planar.cr],
                facts.color,
                facts.keyframe,
                DECODER_PIN,
            )?;
            return Ok(DecodedImage::Cpu(frame));
        }

        let exports = match &s.exports {
            Some(fds) => fds.clone(),
            None => {
                let mut fds = Vec::with_capacity(s.decoder.buffers() as usize);
                for index in 0..s.decoder.buffers() {
                    fds.push(
                        s.decoder
                            .device()
                            .export(index)
                            .context("export a V4L2 picture buffer")?,
                    );
                }
                s.exports.insert(Arc::new(fds)).clone()
            }
        };
        let fd = exports[picture.buffer as usize].as_raw_fd();
        // One memory plane: chroma follows luma at the coded height.
        let chroma_offset = format
            .stride
            .checked_mul(format.height)
            .ok_or_else(|| anyhow!("V4L2 picture geometry overflows"))?;
        Ok(DecodedImage::NativeDmabuf(DmabufFrame {
            width: facts.display.0,
            height: facts.display.1,
            coded_width: format.width,
            coded_height: format.height,
            // The V4L2 codes for NV12 and P010 are the DRM ones.
            fourcc: format.fourcc,
            modifier: DRM_FORMAT_MOD_LINEAR,
            planes: vec![
                DmabufPlane {
                    fd,
                    offset: 0,
                    stride: format.stride,
                },
                DmabufPlane {
                    fd,
                    offset: chroma_offset,
                    stride: format.stride,
                },
            ],
            color: facts.color,
            keyframe: facts.keyframe,
            references_clean: facts.references_clean,
            // A completed request is a finished picture: no fence to wait.
            sync_fds: Vec::new(),
            pool_key: (u64::from(s.pool) << 48) | u64::from(picture.buffer),
            path: DECODER_PIN,
            guard: DrmFrameGuard(FrameGuard::V4l2(V4l2FrameGuard {
                _fds: exports,
                tx: self.release_tx.clone(),
                release: Release {
                    pool: s.pool,
                    generation: 0,
                    index: picture.buffer,
                },
            })),
        }))
    }
}

#[cfg(test)]
mod tests {
    use pf_bitstream::testing::split_h265_aus;
    use pf_bitstream::testing::H265_25FPS;
    use pf_v4l2dec::testing::FakeStateless;

    use super::*;

    /// A decoder that offers the linear format.
    struct Linear(FakeStateless);
    /// The Raspberry Pi's: the column format only.
    struct Columns(FakeStateless);

    macro_rules! fake_node {
        ($name:ident, $format:expr) => {
            impl stateless::Device for $name {
                fn set_output_format(
                    &mut self,
                    a: u32,
                    b: u32,
                    c: u32,
                    d: u32,
                ) -> std::io::Result<()> {
                    self.0.set_output_format(a, b, c, d)
                }
                fn control(&mut self, id: u32) -> std::io::Result<Option<stateless::ControlRange>> {
                    self.0.control(id)
                }
                fn set_controls(
                    &mut self,
                    in_request: bool,
                    controls: &[stateless::Control<'_>],
                ) -> std::io::Result<()> {
                    self.0.set_controls(in_request, controls)
                }
                fn capture_format(
                    &mut self,
                ) -> std::io::Result<pf_v4l2dec::stateful::CaptureFormat> {
                    self.0.capture_format()
                }
                fn capture_formats(&mut self) -> std::io::Result<Vec<u32>> {
                    self.0.capture_formats()
                }
                fn set_capture_format(
                    &mut self,
                    fourcc: u32,
                ) -> std::io::Result<pf_v4l2dec::stateful::CaptureFormat> {
                    self.0.set_capture_format(fourcc)
                }
                fn request_buffers(
                    &mut self,
                    queue: pf_v4l2dec::stateful::Queue,
                    count: u32,
                ) -> std::io::Result<u32> {
                    self.0.request_buffers(queue, count)
                }
                fn stream(
                    &mut self,
                    queue: pf_v4l2dec::stateful::Queue,
                    on: bool,
                ) -> std::io::Result<()> {
                    self.0.stream(queue, on)
                }
                fn queue_output(
                    &mut self,
                    index: u32,
                    data: &[u8],
                    stamp: u64,
                ) -> std::io::Result<()> {
                    self.0.queue_output(index, data, stamp)
                }
                fn queue_capture(&mut self, index: u32) -> std::io::Result<()> {
                    self.0.queue_capture(index)
                }
                fn run_request(&mut self, timeout: std::time::Duration) -> std::io::Result<bool> {
                    self.0.run_request(timeout)
                }
                fn dequeue_output(&mut self) -> std::io::Result<Option<u32>> {
                    self.0.dequeue_output()
                }
                fn dequeue_capture(
                    &mut self,
                ) -> std::io::Result<Option<pf_v4l2dec::stateful::Dequeued>> {
                    self.0.dequeue_capture()
                }
            }

            impl OpenedStateless for $name {
                fn open(_: &Path) -> std::io::Result<$name> {
                    Ok($name(FakeStateless::new(&[$format])))
                }
                fn export(&mut self, _: u32) -> std::io::Result<OwnedFd> {
                    Ok(std::fs::File::open("/dev/null")?.into())
                }
                fn picture(&self, _: u32) -> Option<&[u8]> {
                    Some(self.0.pixels())
                }
            }
        };
    }
    fake_node!(Linear, uapi::V4L2_PIX_FMT_NV12);
    fake_node!(Columns, uapi_stateless::V4L2_PIX_FMT_NV12_COL128);

    /// A linear-format decoder hands its buffers to the presenter, and takes
    /// them back when each frame drops. The fake panics on a reference that
    /// names a buffer decoded over.
    #[test]
    fn a_linear_decoder_ships_dmabufs() {
        let mut d: StatelessHevc<Linear> = StatelessHevc::on_node(PathBuf::from("fake"));
        let aus = split_h265_aus(H265_25FPS);
        let mut shown = 0;
        for (n, au) in aus.iter().enumerate() {
            let Some(image) = d.decode(au).expect("decode") else {
                continue;
            };
            let DecodedImage::NativeDmabuf(f) = image else {
                panic!("unit {n}: a linear picture is a dma-buf");
            };
            assert_eq!(f.path, "native-v4l2");
            assert_eq!(f.fourcc, uapi::V4L2_PIX_FMT_NV12);
            assert_eq!(f.modifier, DRM_FORMAT_MOD_LINEAR);
            assert_eq!(f.planes[1].offset, f.planes[0].stride * f.coded_height);
            assert!(f.width <= f.coded_width && f.height <= f.coded_height);
            shown += 1;
        }
        // The vector reorders, which no host stream does: a unit that bumps
        // several pictures shows the newest and counts the rest.
        let bumped = shown + d.health().dropped as usize;
        assert!(bumped > 240, "{shown} shown, {bumped} of 250 bumped");
        assert!(!d.take_recovery_request());
    }

    /// The Pi's pictures come out by copy, tagged as hardware-decoded.
    #[test]
    fn a_column_decoder_ships_copied_planes() {
        let mut d: StatelessHevc<Columns> = StatelessHevc::on_node(PathBuf::from("fake"));
        let aus = split_h265_aus(H265_25FPS);
        let mut shown = 0;
        for au in &aus {
            let Some(image) = d.decode(au).expect("decode") else {
                continue;
            };
            assert_eq!(image.path_label(), "native-v4l2");
            let DecodedImage::Cpu(f) = image else {
                panic!("a column picture is copied out");
            };
            let (w, h) = (f.width as usize, f.height as usize);
            assert_eq!(f.plane(0).len(), w * h);
            assert_eq!(f.plane(1).len(), w.div_ceil(2) * h.div_ceil(2));
            shown += 1;
        }
        assert!(shown > 100, "{shown} of 250 shown");
    }

    /// A lost picture makes what predicts from it undecodable: withheld with
    /// a keyframe request, never an error that would demote the rung.
    #[test]
    fn a_lost_reference_is_loss_not_a_decoder_fault() {
        let mut d: StatelessHevc<Linear> = StatelessHevc::on_node(PathBuf::from("fake"));
        let aus = split_h265_aus(H265_25FPS);
        d.decode(aus[0]).expect("the IDR decodes");
        // Three units never arrive; what follows predicts from them.
        let mut withheld = 0;
        for au in &aus[4..16] {
            if d.decode(au).expect("loss is not an error").is_none() {
                withheld += 1;
            }
        }
        assert!(withheld > 0, "something predicted from the lost picture");
        assert!(d.take_recovery_request());
    }
}
