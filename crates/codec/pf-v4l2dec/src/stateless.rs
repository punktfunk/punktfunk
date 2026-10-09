//! The stateless decoder flow: the client parses the stream and hands the
//! driver one picture per media request — the bitstream on the OUTPUT queue,
//! the headers as controls, a CAPTURE buffer to decode into.
//!
//! A reference picture is a CAPTURE buffer the driver finds by timestamp, so
//! the client owns the decoded picture buffer: [`Ledger`] keeps which buffer
//! holds which picture and refuses to hand one out while it is a reference,
//! awaits display, or is still with the consumer. Those three end at
//! different times.
//!
//! [`HevcDecoder`] ties a [`Session`] and a ledger to the HEVC control fill.
//! Decoding is synchronous — one request in flight — which is what a stream
//! with one picture per access unit and no reordering needs. A request that
//! does not complete is [`Error::Stalled`]; the session is then unusable.

use std::time::Duration;

use pf_bitstream::h265::AuPlan;
use pf_bitstream::h265::PicId;

use crate::hevc;
use crate::stateful::fourcc_name;
use crate::stateful::CaptureFormat;
use crate::stateful::Dequeued;
use crate::stateful::Queue;
use crate::uapi_stateless as uapi;
use crate::uapi_stateless::V4l2CtrlHevcDecodeParams;
use crate::uapi_stateless::V4l2CtrlHevcExtSpsLtRps;
use crate::uapi_stateless::V4l2CtrlHevcExtSpsStRps;
use crate::uapi_stateless::V4l2CtrlHevcPps;
use crate::uapi_stateless::V4l2CtrlHevcScalingMatrix;
use crate::uapi_stateless::V4l2CtrlHevcSliceParams;
use crate::uapi_stateless::V4l2CtrlHevcSps;

/// Pictures beyond the stream's DPB: the one being decoded, plus what the
/// consumer may still hold.
const POOL_HEADROOM: u32 = 4;

/// How long one picture may take. Hardware answers in milliseconds.
const REQUEST_TIMEOUT: Duration = Duration::from_millis(500);

/// One control of a request, typed so the ioctl side can size it.
#[derive(Debug, Clone, Copy)]
pub enum Control<'a> {
    Value { id: u32, value: i32 },
    HevcSps(&'a V4l2CtrlHevcSps),
    HevcPps(&'a V4l2CtrlHevcPps),
    HevcDecodeParams(&'a V4l2CtrlHevcDecodeParams),
    HevcSliceParams(&'a [V4l2CtrlHevcSliceParams]),
    HevcScalingMatrix(&'a V4l2CtrlHevcScalingMatrix),
    HevcEntryPoints(&'a [u32]),
    HevcStRps(&'a [V4l2CtrlHevcExtSpsStRps]),
    HevcLtRps(&'a [V4l2CtrlHevcExtSpsLtRps]),
}

impl Control<'_> {
    pub fn id(&self) -> u32 {
        match self {
            Control::Value { id, .. } => *id,
            Control::HevcSps(_) => uapi::V4L2_CID_STATELESS_HEVC_SPS,
            Control::HevcPps(_) => uapi::V4L2_CID_STATELESS_HEVC_PPS,
            Control::HevcDecodeParams(_) => uapi::V4L2_CID_STATELESS_HEVC_DECODE_PARAMS,
            Control::HevcSliceParams(_) => uapi::V4L2_CID_STATELESS_HEVC_SLICE_PARAMS,
            Control::HevcScalingMatrix(_) => uapi::V4L2_CID_STATELESS_HEVC_SCALING_MATRIX,
            Control::HevcEntryPoints(_) => uapi::V4L2_CID_STATELESS_HEVC_ENTRY_POINT_OFFSETS,
            Control::HevcStRps(_) => uapi::V4L2_CID_STATELESS_HEVC_EXT_SPS_ST_RPS,
            Control::HevcLtRps(_) => uapi::V4L2_CID_STATELESS_HEVC_EXT_SPS_LT_RPS,
        }
    }
}

/// A control's range, from the driver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ControlRange {
    pub minimum: i64,
    pub maximum: i64,
}

/// The ioctl surface of a stateless decoder node and its request. One
/// request exists at a time; the device owns it.
pub trait Device {
    fn set_output_format(
        &mut self,
        fourcc: u32,
        width: u32,
        height: u32,
        buffer_size: u32,
    ) -> std::io::Result<()>;
    /// `None` when the driver has no such control.
    fn control(&mut self, id: u32) -> std::io::Result<Option<ControlRange>>;
    /// On the device itself, or on the pending request.
    fn set_controls(&mut self, in_request: bool, controls: &[Control<'_>]) -> std::io::Result<()>;
    fn capture_format(&mut self) -> std::io::Result<CaptureFormat>;
    fn capture_formats(&mut self) -> std::io::Result<Vec<u32>>;
    fn set_capture_format(&mut self, fourcc: u32) -> std::io::Result<CaptureFormat>;
    fn request_buffers(&mut self, queue: Queue, count: u32) -> std::io::Result<u32>;
    fn stream(&mut self, queue: Queue, on: bool) -> std::io::Result<()>;
    /// Queue the bitstream into the pending request.
    fn queue_output(&mut self, index: u32, data: &[u8], stamp: u64) -> std::io::Result<()>;
    fn queue_capture(&mut self, index: u32) -> std::io::Result<()>;
    /// Queue the request and wait for it. `false` is a timeout; on `true`
    /// the request is ready to be filled again.
    fn run_request(&mut self, timeout: Duration) -> std::io::Result<bool>;
    fn dequeue_output(&mut self) -> std::io::Result<Option<u32>>;
    fn dequeue_capture(&mut self) -> std::io::Result<Option<Dequeued>>;
}

#[derive(Debug)]
pub enum Error {
    Device {
        op: &'static str,
        source: std::io::Error,
    },
    /// The driver offers none of the wanted picture formats.
    NoUsableFormat {
        offered: Vec<u32>,
    },
    /// The driver decodes slice by slice only.
    NoFrameMode,
    /// Every picture buffer is a reference, awaits display, or is held.
    PoolExhausted {
        buffers: u32,
    },
    /// The request did not complete, or completed without its buffers.
    Stalled,
    Fill(hevc::FillError),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Device { op, source } => write!(f, "{op}: {source}"),
            Error::NoUsableFormat { offered } => {
                write!(f, "no usable picture format among")?;
                for code in offered {
                    write!(f, " {}", fourcc_name(*code))?;
                }
                Ok(())
            }
            Error::NoFrameMode => write!(f, "the decoder has no frame-based mode"),
            Error::PoolExhausted { buffers } => {
                write!(f, "all {buffers} picture buffers are in use")
            }
            Error::Stalled => write!(f, "the decode request did not complete"),
            Error::Fill(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Device { source, .. } => Some(source),
            Error::Fill(e) => Some(e),
            _ => None,
        }
    }
}

fn op<T>(op: &'static str, r: std::io::Result<T>) -> Result<T, Error> {
    r.map_err(|source| Error::Device { op, source })
}

/// An opened, streaming stateless decoder.
pub struct Session<D: Device> {
    dev: D,
    format: CaptureFormat,
    buffers: u32,
}

impl<D: Device> Session<D> {
    /// Configure both queues and start them. `setup` goes to the device
    /// before the picture format is read: drivers derive it from the SPS.
    pub fn open(
        mut dev: D,
        coded: u32,
        width: u32,
        height: u32,
        setup: &[Control<'_>],
        wanted: &[u32],
        buffers: u32,
    ) -> Result<Session<D>, Error> {
        let buffer_size = width.saturating_mul(height).max(2 << 20);
        op(
            "set the input format",
            dev.set_output_format(coded, width, height, buffer_size),
        )?;
        op("set the stream parameters", dev.set_controls(false, setup))?;
        let mut format = op("read the picture format", dev.capture_format())?;
        if !wanted.contains(&format.fourcc) {
            let offered = op("list picture formats", dev.capture_formats())?;
            let Some(fourcc) = wanted.iter().copied().find(|w| offered.contains(w)) else {
                return Err(Error::NoUsableFormat { offered });
            };
            format = op("set the picture format", dev.set_capture_format(fourcc))?;
        }
        if !wanted.contains(&format.fourcc) || format.planes != 1 {
            return Err(Error::NoUsableFormat {
                offered: vec![format.fourcc],
            });
        }
        // One input buffer: a request is decoded before the next is built.
        op(
            "allocate the input buffer",
            dev.request_buffers(Queue::Output, 1),
        )?;
        let buffers = op(
            "allocate picture buffers",
            dev.request_buffers(Queue::Capture, buffers),
        )?;
        op("start the input queue", dev.stream(Queue::Output, true))?;
        op("start the picture queue", dev.stream(Queue::Capture, true))?;
        Ok(Session {
            dev,
            format,
            buffers,
        })
    }

    pub fn device(&mut self) -> &mut D {
        &mut self.dev
    }

    pub fn format(&self) -> CaptureFormat {
        self.format
    }

    pub fn buffers(&self) -> u32 {
        self.buffers
    }

    /// Decode one picture into `target`. Returns whether the driver flagged
    /// the result as corrupt.
    pub fn decode(
        &mut self,
        target: u32,
        controls: &[Control<'_>],
        data: &[u8],
        stamp: u64,
    ) -> Result<bool, Error> {
        op("queue the bitstream", self.dev.queue_output(0, data, stamp))?;
        op(
            "set the picture parameters",
            self.dev.set_controls(true, controls),
        )?;
        op("queue the picture buffer", self.dev.queue_capture(target))?;
        if !op("run the request", self.dev.run_request(REQUEST_TIMEOUT))? {
            return Err(Error::Stalled);
        }
        if op("dequeue the bitstream", self.dev.dequeue_output())?.is_none() {
            return Err(Error::Stalled);
        }
        match op("dequeue the picture", self.dev.dequeue_capture())? {
            Some(done) if done.index == target => Ok(done.error),
            _ => Err(Error::Stalled),
        }
    }
}

/// A decoded picture awaiting display.
#[derive(Debug, Clone, Copy)]
struct Pending<F> {
    id: PicId,
    buffer: u32,
    facts: F,
    corrupt: bool,
}

/// Which CAPTURE buffer holds which picture, and who still needs it.
#[derive(Debug)]
pub struct Ledger<F> {
    /// Pictures in the decoded picture buffer: `(id, buffer, stamp)`.
    stored: Vec<(PicId, u32, u64)>,
    pending: Vec<Pending<F>>,
    /// Shown and not yet released by the consumer.
    held: Vec<bool>,
}

/// A picture ready for display. Its buffer is held until
/// [`HevcDecoder::release`].
#[derive(Debug, Clone, Copy)]
pub struct Shown<F> {
    pub buffer: u32,
    pub facts: F,
    /// The driver flagged this picture as possibly corrupt.
    pub corrupt: bool,
}

impl<F> Ledger<F> {
    pub fn new(buffers: u32) -> Ledger<F> {
        Ledger {
            stored: Vec::new(),
            pending: Vec::new(),
            held: vec![false; buffers as usize],
        }
    }

    /// A buffer nothing needs: not a reference, not pending, not held.
    pub fn free(&self) -> Option<u32> {
        (0..self.held.len() as u32).find(|b| {
            !self.held[*b as usize]
                && !self.stored.iter().any(|(_, buffer, _)| buffer == b)
                && !self.pending.iter().any(|p| p.buffer == *b)
        })
    }

    /// The stamp the picture `id` was decoded with, while it is stored.
    pub fn stamp_of(&self, id: PicId) -> Option<u64> {
        self.stored
            .iter()
            .find(|(stored, _, _)| *stored == id)
            .map(|(_, _, stamp)| *stamp)
    }

    fn store(&mut self, id: PicId, buffer: u32, stamp: u64, facts: F, corrupt: bool) {
        self.stored.push((id, buffer, stamp));
        self.pending.push(Pending {
            id,
            buffer,
            facts,
            corrupt,
        });
    }

    /// Claim the outputs in display order, then retire what left the DPB —
    /// in that order, because one access unit can do both to a picture.
    fn settle(&mut self, outputs: &[PicId], removed: &[PicId]) -> Vec<Shown<F>> {
        let mut shown = Vec::with_capacity(outputs.len());
        for id in outputs {
            if let Some(at) = self.pending.iter().position(|p| p.id == *id) {
                let p = self.pending.remove(at);
                self.held[p.buffer as usize] = true;
                shown.push(Shown {
                    buffer: p.buffer,
                    facts: p.facts,
                    corrupt: p.corrupt,
                });
            }
        }
        for id in removed {
            self.stored.retain(|(stored, _, _)| stored != id);
            // Left the DPB without ever being output.
            self.pending.retain(|p| p.id != *id);
        }
        shown
    }

    pub fn release(&mut self, buffer: u32) {
        if let Some(held) = self.held.get_mut(buffer as usize) {
            *held = false;
        }
    }
}

/// HEVC over a stateless decoder: one planned access unit in, the pictures
/// it makes displayable out.
pub struct HevcDecoder<D: Device, F> {
    session: Session<D>,
    ledger: Ledger<F>,
    annex_b: bool,
    /// Controls the driver has beyond the four every HEVC decoder takes.
    scaling: bool,
    entry_points: bool,
    st_rps: bool,
    lt_rps: bool,
    next_stamp: u64,
}

impl<D: Device, F> HevcDecoder<D, F> {
    /// Open for the stream `plan` starts. `wanted` lists the picture formats
    /// the caller can display, most wanted first.
    pub fn open(
        mut dev: D,
        plan: &AuPlan,
        au: &[u8],
        wanted: &[u32],
    ) -> Result<HevcDecoder<D, F>, Error> {
        let mode = op(
            "query the decode mode",
            dev.control(uapi::V4L2_CID_STATELESS_HEVC_DECODE_MODE),
        )?;
        let frame = uapi::V4L2_STATELESS_HEVC_DECODE_MODE_FRAME_BASED;
        if mode.is_some_and(|m| m.maximum < frame) {
            return Err(Error::NoFrameMode);
        }
        let start_code = op(
            "query the start-code mode",
            dev.control(uapi::V4L2_CID_STATELESS_HEVC_START_CODE),
        )?;
        // Without the control the default is start codes; with it, take the
        // bare NAL units where the driver allows them.
        let annex_b =
            start_code.is_none_or(|r| r.minimum > uapi::V4L2_STATELESS_HEVC_START_CODE_NONE);
        let mut has = |id: u32| op("query a control", dev.control(id)).map(|r| r.is_some());
        let scaling = has(uapi::V4L2_CID_STATELESS_HEVC_SCALING_MATRIX)?;
        let entry_points = has(uapi::V4L2_CID_STATELESS_HEVC_ENTRY_POINT_OFFSETS)?;
        let st_rps = has(uapi::V4L2_CID_STATELESS_HEVC_EXT_SPS_ST_RPS)?;
        let lt_rps = has(uapi::V4L2_CID_STATELESS_HEVC_EXT_SPS_LT_RPS)?;

        let first = hevc::fill(plan, au, |_| None, annex_b).map_err(Error::Fill)?;
        let mut setup = Vec::with_capacity(3);
        if mode.is_some() {
            setup.push(Control::Value {
                id: uapi::V4L2_CID_STATELESS_HEVC_DECODE_MODE,
                value: frame as i32,
            });
        }
        if start_code.is_some() {
            setup.push(Control::Value {
                id: uapi::V4L2_CID_STATELESS_HEVC_START_CODE,
                value: i32::from(annex_b),
            });
        }
        setup.push(Control::HevcSps(&first.sps));

        let pic = &plan.picture;
        let buffers = pic.max_dpb_frames as u32 + 1 + POOL_HEADROOM;
        let session = Session::open(
            dev,
            uapi::V4L2_PIX_FMT_HEVC_SLICE,
            pic.coded_width,
            pic.coded_height,
            &setup,
            wanted,
            buffers,
        )?;
        let ledger = Ledger::new(session.buffers());
        Ok(HevcDecoder {
            session,
            ledger,
            annex_b,
            scaling,
            entry_points,
            st_rps,
            lt_rps,
            next_stamp: 1,
        })
    }

    pub fn device(&mut self) -> &mut D {
        self.session.device()
    }

    pub fn format(&self) -> CaptureFormat {
        self.session.format()
    }

    /// Picture buffers in the pool.
    pub fn buffers(&self) -> u32 {
        self.session.buffers()
    }

    /// Decode one access unit and return what it makes displayable. On an
    /// error the picture was not stored: later units that reference it fail
    /// to fill, until a keyframe.
    pub fn decode(&mut self, plan: &AuPlan, au: &[u8], facts: F) -> Result<Vec<Shown<F>>, Error> {
        let target = self.ledger.free().ok_or(Error::PoolExhausted {
            buffers: self.session.buffers(),
        })?;
        let ledger = &self.ledger;
        // The kernel matches references by the buffer's timestamp in ns.
        let req = hevc::fill(
            plan,
            au,
            |id| ledger.stamp_of(id).map(|us| us * 1000),
            self.annex_b,
        )
        .map_err(Error::Fill)?;

        let mut controls = vec![
            Control::HevcSps(&req.sps),
            Control::HevcPps(&req.pps),
            Control::HevcDecodeParams(&req.decode),
            Control::HevcSliceParams(&req.slices),
        ];
        if self.scaling {
            controls.push(Control::HevcScalingMatrix(&req.scaling));
        }
        if self.entry_points && !req.entry_points.is_empty() {
            controls.push(Control::HevcEntryPoints(&req.entry_points));
        }
        if self.st_rps && !req.st_rps.is_empty() {
            controls.push(Control::HevcStRps(&req.st_rps));
        }
        if self.lt_rps && !req.lt_rps.is_empty() {
            controls.push(Control::HevcLtRps(&req.lt_rps));
        }

        let stamp = self.next_stamp;
        self.next_stamp += 1;
        let corrupt = self.session.decode(target, &controls, &req.data, stamp)?;
        if let Some(id) = plan.dpb.stored {
            self.ledger.store(id, target, stamp, facts, corrupt);
        }
        Ok(self.ledger.settle(&plan.dpb.outputs, &plan.dpb.removed))
    }

    /// The consumer is done with a shown picture's buffer.
    pub fn release(&mut self, buffer: u32) {
        self.ledger.release(buffer);
    }
}

#[cfg(test)]
mod tests {
    use pf_bitstream::h265::H265Planner;
    use pf_bitstream::testing::split_h265_aus;
    use pf_bitstream::testing::H265_25FPS;

    use super::*;
    use crate::testing::FakeStateless;
    use crate::uapi::V4L2_PIX_FMT_NV12;

    fn decode_all(annex_only: bool, hold: usize) -> (FakeStateless, usize) {
        let aus = split_h265_aus(H265_25FPS);
        let mut planner = H265Planner::new();
        let mut decoder: Option<HevcDecoder<FakeStateless, usize>> = None;
        let mut shown = 0usize;
        let mut held = std::collections::VecDeque::new();
        for (n, au) in aus.iter().enumerate() {
            let plan = planner.plan_au(au).expect("the vector plans");
            let d = decoder.get_or_insert_with(|| {
                let mut fake = FakeStateless::new(&[V4L2_PIX_FMT_NV12]);
                fake.annex_b_only = annex_only;
                HevcDecoder::open(fake, &plan, au, &[V4L2_PIX_FMT_NV12]).expect("open")
            });
            for picture in d.decode(&plan, au, n).expect("decode") {
                assert!(!picture.corrupt);
                shown += 1;
                held.push_back(picture.buffer);
                // A consumer that keeps the last `hold` pictures.
                if held.len() > hold {
                    d.release(held.pop_front().unwrap());
                }
            }
        }
        let mut d = decoder.expect("the vector has pictures");
        (std::mem::take(d.device()), shown)
    }

    /// The fake checks every request: each reference names a buffer that
    /// still holds that picture, and the target is not one of them.
    #[test]
    fn every_reference_is_a_live_buffer() {
        let (fake, shown) = decode_all(false, 0);
        assert_eq!(fake.decoded, 250);
        assert!(fake.references > 0, "the vector has inter pictures");
        // The tail stays in the DPB: the stream ends without a flush.
        assert!(shown > 240 && shown <= 250, "{shown} shown");
        assert!(!fake.saw_start_codes);
    }

    #[test]
    fn a_consumer_holding_pictures_never_has_one_overwritten() {
        let (fake, _) = decode_all(false, 3);
        assert_eq!(fake.decoded, 250);
    }

    #[test]
    fn a_driver_that_wants_start_codes_gets_them() {
        let (fake, _) = decode_all(true, 0);
        assert_eq!(fake.decoded, 250);
        assert!(fake.saw_start_codes);
    }

    #[test]
    fn a_pool_with_nothing_free_is_refused_not_overrun() {
        let aus = split_h265_aus(H265_25FPS);
        let mut planner = H265Planner::new();
        let plan = planner.plan_au(aus[0]).unwrap();
        let fake = FakeStateless::new(&[V4L2_PIX_FMT_NV12]);
        let mut d: HevcDecoder<FakeStateless, ()> =
            HevcDecoder::open(fake, &plan, aus[0], &[V4L2_PIX_FMT_NV12]).unwrap();
        // Nothing is ever released: the pool fills with held pictures.
        let mut result = d.decode(&plan, aus[0], ()).map(|_| ());
        for au in &aus[1..] {
            if result.is_err() {
                break;
            }
            let plan = planner.plan_au(au).unwrap();
            result = d.decode(&plan, au, ()).map(|_| ());
        }
        assert!(matches!(result, Err(Error::PoolExhausted { .. })));
    }

    #[test]
    fn a_slice_only_driver_is_refused() {
        let aus = split_h265_aus(H265_25FPS);
        let plan = H265Planner::new().plan_au(aus[0]).unwrap();
        let mut fake = FakeStateless::new(&[V4L2_PIX_FMT_NV12]);
        fake.slice_based_only = true;
        let opened: Result<HevcDecoder<FakeStateless, ()>, _> =
            HevcDecoder::open(fake, &plan, aus[0], &[V4L2_PIX_FMT_NV12]);
        assert!(matches!(opened, Err(Error::NoFrameMode)));
    }
}
