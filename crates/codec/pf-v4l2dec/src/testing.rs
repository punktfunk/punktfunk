//! Decoders that follow the kernel's contracts without a device, for this
//! crate's tests and the client rungs'. They panic where a real driver would
//! corrupt a picture: a client bug, not a state.

use std::collections::VecDeque;
use std::time::Duration;

use crate::stateful::CaptureFormat;
use crate::stateful::Dequeued;
use crate::stateful::Device;
use crate::stateful::Event;
use crate::stateful::Interest;
use crate::stateful::Queue;

/// Decodes one picture per access unit, in order, copying its stamp. Nothing
/// decodes until the CAPTURE queue runs, and a source change pauses it again.
/// Panics on a buffer queued twice or out of range: a client bug, not a state.
#[derive(Default)]
pub struct FakeDecoder {
    /// Coded size the "stream" carries; a change raises a source change.
    pub stream: (u32, u32),
    /// The size last announced. `None` makes the next unit announce again.
    pub announced: Option<(u32, u32)>,
    /// The driver stopped consuming input.
    pub frozen: bool,
    /// CAPTURE allocations: `"allocate"` and `"free"`, in order.
    pub log: Vec<&'static str>,
    offered: Vec<u32>,
    capture_fourcc: u32,
    inputs: u32,
    /// Queued access units, oldest first.
    pending: VecDeque<(u32, u64)>,
    returned_inputs: VecDeque<u32>,
    capture_streaming: bool,
    capture_buffers: u32,
    queued_capture: VecDeque<u32>,
    done: VecDeque<Dequeued>,
    events: VecDeque<Event>,
}

impl FakeDecoder {
    pub fn new(width: u32, height: u32, offered: &[u32]) -> FakeDecoder {
        FakeDecoder {
            stream: (width, height),
            offered: offered.to_vec(),
            ..FakeDecoder::default()
        }
    }

    /// Resume a frozen decoder and let it work through its queue.
    pub fn thaw(&mut self) {
        self.frozen = false;
        self.run();
    }

    /// Decode whatever the contract allows right now.
    fn run(&mut self) {
        if self.frozen {
            return;
        }
        while let Some(&(index, stamp)) = self.pending.front() {
            if self.announced != Some(self.stream) {
                self.announced = Some(self.stream);
                self.capture_streaming = false;
                self.events.push_back(Event::SourceChange);
                return;
            }
            if !self.capture_streaming {
                return;
            }
            let Some(target) = self.queued_capture.pop_front() else {
                return;
            };
            self.pending.pop_front();
            self.returned_inputs.push_back(index);
            self.done.push_back(Dequeued {
                index: target,
                stamp,
                error: false,
                empty: false,
            });
        }
    }
}

impl Device for FakeDecoder {
    fn set_output_format(&mut self, _: u32, _: u32, _: u32, _: u32) -> std::io::Result<()> {
        Ok(())
    }

    fn subscribe_source_change(&mut self) -> std::io::Result<()> {
        Ok(())
    }

    fn request_buffers(&mut self, queue: Queue, count: u32) -> std::io::Result<u32> {
        match queue {
            Queue::Output => self.inputs = count,
            Queue::Capture => {
                self.log.push(if count == 0 { "free" } else { "allocate" });
                self.capture_buffers = count;
                self.queued_capture.clear();
                self.done.clear();
            }
        }
        Ok(count)
    }

    fn stream(&mut self, queue: Queue, on: bool) -> std::io::Result<()> {
        if queue == Queue::Capture {
            self.capture_streaming = on;
            if !on {
                // STREAMOFF returns every buffer to the client, undecoded.
                self.queued_capture.clear();
                self.done.clear();
            }
            self.run();
        }
        Ok(())
    }

    fn queue_output(&mut self, index: u32, _: &[u8], stamp: u64) -> std::io::Result<()> {
        assert!(index < self.inputs, "input buffer {index} is not allocated");
        self.pending.push_back((index, stamp));
        self.run();
        Ok(())
    }

    fn dequeue_output(&mut self) -> std::io::Result<Option<u32>> {
        Ok(self.returned_inputs.pop_front())
    }

    fn queue_capture(&mut self, index: u32) -> std::io::Result<()> {
        assert!(
            index < self.capture_buffers,
            "buffer {index} is not in the pool"
        );
        assert!(
            !self.queued_capture.contains(&index),
            "buffer {index} queued twice"
        );
        self.queued_capture.push_back(index);
        self.run();
        Ok(())
    }

    fn dequeue_capture(&mut self) -> std::io::Result<Option<Dequeued>> {
        Ok(self.done.pop_front())
    }

    fn dequeue_event(&mut self) -> std::io::Result<Option<Event>> {
        Ok(self.events.pop_front())
    }

    fn capture_format(&mut self) -> std::io::Result<CaptureFormat> {
        let (width, height) = self.announced.unwrap_or(self.stream);
        Ok(CaptureFormat {
            fourcc: self.capture_fourcc,
            width,
            height,
            stride: width,
            planes: 1,
        })
    }

    fn capture_formats(&mut self) -> std::io::Result<Vec<u32>> {
        Ok(self.offered.clone())
    }

    fn set_capture_format(&mut self, fourcc: u32) -> std::io::Result<CaptureFormat> {
        self.capture_fourcc = fourcc;
        self.capture_format()
    }

    fn min_capture_buffers(&mut self) -> std::io::Result<u32> {
        Ok(2)
    }

    fn wait(&mut self, _: Interest, _: Duration) -> std::io::Result<()> {
        Ok(())
    }
}

/// A stateless decoder that checks each request the way hardware would fail
/// on it: every reference must name a buffer that still holds that picture.
#[derive(Default)]
pub struct FakeStateless {
    /// The driver takes slices with start codes only.
    pub annex_b_only: bool,
    /// The driver has no frame-based mode.
    pub slice_based_only: bool,
    /// Requests decoded.
    pub decoded: usize,
    /// References resolved across all requests.
    pub references: usize,
    pub saw_start_codes: bool,
    offered: Vec<u32>,
    capture_fourcc: u32,
    size: (u32, u32),
    start_codes: bool,
    /// The picture each CAPTURE buffer holds, by timestamp in ns.
    pictures: Vec<Option<u64>>,
    target: Option<u32>,
    bitstream: Option<(Vec<u8>, u64)>,
    /// From the pending request: DPB timestamps and each slice's
    /// `(bit_size, data_byte_offset)`.
    dpb: Option<Vec<u64>>,
    slices: Option<Vec<(u32, u32)>>,
    parameter_sets: u8,
    done: Option<crate::stateful::Dequeued>,
    input_done: bool,
    /// What every picture buffer reads as: nothing is decoded.
    pixels: Vec<u8>,
}

impl FakeStateless {
    pub fn new(offered: &[u32]) -> FakeStateless {
        FakeStateless {
            offered: offered.to_vec(),
            capture_fourcc: offered.first().copied().unwrap_or(0),
            ..FakeStateless::default()
        }
    }

    /// The bytes of any picture buffer: zeros, sized as the format says.
    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }

    /// The picture geometry a driver would report for the current format.
    fn geometry(&self) -> CaptureFormat {
        let (width, height) = self.size;
        if self.capture_fourcc == crate::uapi_stateless::V4L2_PIX_FMT_NV12_COL128 {
            // The Raspberry Pi driver: whole columns, rows padded to 8, and
            // `bytesperline` as the column height in rows.
            let height = height.next_multiple_of(8);
            return CaptureFormat {
                fourcc: self.capture_fourcc,
                width: width.next_multiple_of(128),
                height,
                stride: height * 3 / 2,
                planes: 1,
            };
        }
        CaptureFormat {
            fourcc: self.capture_fourcc,
            width,
            height,
            stride: width,
            planes: 1,
        }
    }
}

impl crate::stateless::Device for FakeStateless {
    fn set_output_format(
        &mut self,
        _: u32,
        width: u32,
        height: u32,
        _: u32,
    ) -> std::io::Result<()> {
        self.size = (width, height);
        Ok(())
    }

    fn control(&mut self, id: u32) -> std::io::Result<Option<crate::stateless::ControlRange>> {
        use crate::stateless::ControlRange;
        use crate::uapi_stateless as uapi;
        Ok(match id {
            uapi::V4L2_CID_STATELESS_HEVC_DECODE_MODE => Some(ControlRange {
                minimum: 0,
                maximum: i64::from(!self.slice_based_only),
            }),
            uapi::V4L2_CID_STATELESS_HEVC_START_CODE => Some(ControlRange {
                minimum: i64::from(self.annex_b_only),
                maximum: 1,
            }),
            uapi::V4L2_CID_STATELESS_HEVC_SCALING_MATRIX
            | uapi::V4L2_CID_STATELESS_HEVC_ENTRY_POINT_OFFSETS => Some(ControlRange {
                minimum: 0,
                maximum: 0,
            }),
            _ => None,
        })
    }

    fn set_controls(
        &mut self,
        in_request: bool,
        controls: &[crate::stateless::Control<'_>],
    ) -> std::io::Result<()> {
        use crate::stateless::Control;
        use crate::uapi_stateless as uapi;
        for control in controls {
            match control {
                Control::Value { id, value } => {
                    assert!(!in_request, "mode controls belong to the device");
                    if *id == uapi::V4L2_CID_STATELESS_HEVC_START_CODE {
                        self.start_codes = *value == 1;
                    }
                    if *id == uapi::V4L2_CID_STATELESS_HEVC_DECODE_MODE {
                        assert_eq!(*value, 1, "frame-based decoding");
                    }
                }
                Control::HevcSps(sps) => {
                    self.size = (
                        u32::from(sps.pic_width_in_luma_samples),
                        u32::from(sps.pic_height_in_luma_samples),
                    );
                    self.parameter_sets |= 1;
                }
                Control::HevcPps(_) => self.parameter_sets |= 2,
                Control::HevcDecodeParams(params) => {
                    let n = usize::from(params.num_active_dpb_entries);
                    self.dpb = Some(params.dpb[..n].iter().map(|e| e.timestamp).collect());
                }
                Control::HevcSliceParams(slices) => {
                    self.slices = Some(
                        slices
                            .iter()
                            .map(|s| (s.bit_size, s.data_byte_offset))
                            .collect(),
                    );
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn capture_format(&mut self) -> std::io::Result<CaptureFormat> {
        Ok(self.geometry())
    }

    fn capture_formats(&mut self) -> std::io::Result<Vec<u32>> {
        Ok(self.offered.clone())
    }

    fn set_capture_format(&mut self, fourcc: u32) -> std::io::Result<CaptureFormat> {
        self.capture_fourcc = fourcc;
        Ok(self.geometry())
    }

    fn request_buffers(&mut self, queue: Queue, count: u32) -> std::io::Result<u32> {
        if queue == Queue::Capture {
            self.pictures = vec![None; count as usize];
            let g = self.geometry();
            // Enough for either layout: columns of `stride` rows, or rows of `stride` bytes.
            self.pixels = vec![0; (g.stride * g.width.max(g.height) * 2) as usize];
        }
        Ok(count)
    }

    fn stream(&mut self, _: Queue, _: bool) -> std::io::Result<()> {
        Ok(())
    }

    fn queue_output(&mut self, index: u32, data: &[u8], stamp: u64) -> std::io::Result<()> {
        assert_eq!(index, 0, "one input buffer");
        assert!(self.bitstream.is_none(), "a request is already pending");
        self.bitstream = Some((data.to_vec(), stamp));
        Ok(())
    }

    fn queue_capture(&mut self, index: u32) -> std::io::Result<()> {
        assert!(self.target.is_none(), "one picture buffer per request");
        // Queued for decoding: whatever it held is gone.
        *self
            .pictures
            .get_mut(index as usize)
            .expect("a buffer of the pool") = None;
        self.target = Some(index);
        Ok(())
    }

    fn run_request(&mut self, _: Duration) -> std::io::Result<bool> {
        let (data, stamp) = self.bitstream.take().expect("a request needs a bitstream");
        let target = self
            .target
            .take()
            .expect("a request needs a picture buffer");
        let dpb = self.dpb.take().expect("decode params are required");
        let slices = self.slices.take().expect("slice params are required");
        assert_eq!(
            self.parameter_sets, 3,
            "SPS and PPS travel with the request"
        );
        self.parameter_sets = 0;
        for timestamp in &dpb {
            let holder = self.pictures.iter().position(|p| *p == Some(*timestamp));
            assert!(
                holder.is_some(),
                "request {}: reference {timestamp} is in no buffer",
                self.decoded
            );
            self.references += 1;
        }
        let mut at = 0usize;
        for (bit_size, data_byte_offset) in slices {
            let bytes = (bit_size / 8) as usize;
            let slice = &data[at..at + bytes];
            let prefixed = slice.starts_with(&[0, 0, 1]);
            assert_eq!(prefixed, self.start_codes, "start code per the mode set");
            self.saw_start_codes |= prefixed;
            assert!((data_byte_offset as usize) < bytes);
            at += bytes;
        }
        assert_eq!(at, data.len(), "the slices cover the buffer");
        self.pictures[target as usize] = Some(stamp * 1000);
        self.decoded += 1;
        self.input_done = true;
        self.done = Some(Dequeued {
            index: target,
            stamp,
            error: false,
            empty: false,
        });
        Ok(true)
    }

    fn dequeue_output(&mut self) -> std::io::Result<Option<u32>> {
        Ok(std::mem::take(&mut self.input_done).then_some(0))
    }

    fn dequeue_capture(&mut self) -> std::io::Result<Option<Dequeued>> {
        Ok(self.done.take())
    }
}
