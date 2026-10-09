//! The stateful (memory-to-memory) decoder flow: compressed access units in
//! on the OUTPUT queue, decoded pictures out on the CAPTURE queue.
//!
//! The driver parses the bitstream and announces the picture format with a
//! source-change event; only then can the CAPTURE queue be allocated. The same
//! event arrives again on a mid-stream resolution change, and some drivers
//! raise it around keyframes with nothing changed. Both are answered by
//! stopping the CAPTURE queue: pictures not yet dequeued are dropped, which the
//! host's keyframe on a resize makes harmless.
//!
//! A CAPTURE buffer handed out by [`Stateful::pump`] belongs to the consumer
//! until [`Stateful::release`]; `generation` keeps a buffer of a replaced pool
//! from being queued into the new one. Nothing here blocks past its timeout: a
//! decoder that stops answering surfaces as [`Error::Stalled`].

use std::time::Duration;
use std::time::Instant;

/// Compressed-input buffers. Hosts send one access unit per frame and the
/// decoder returns each buffer once parsed, so a handful covers a burst.
const OUTPUT_BUFFERS: u32 = 4;

/// Pictures the presenter may hold beyond what the decoder needs for itself.
const CAPTURE_HEADROOM: u32 = 4;

/// Used when the driver has no `MIN_BUFFERS_FOR_CAPTURE` control.
const CAPTURE_MIN_FALLBACK: u32 = 4;

/// How long [`Stateful::submit`] waits for the decoder to return an input buffer.
const SUBMIT_WAIT: Duration = Duration::from_millis(50);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Queue {
    Output,
    Capture,
}

/// The decoded-picture format the driver reports or accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureFormat {
    pub fourcc: u32,
    /// Coded size: the buffer's addressable extent, not the visible picture.
    pub width: u32,
    pub height: u32,
    /// Bytes per row of the first memory plane.
    pub stride: u32,
    /// Memory planes per buffer. The linear formats this rung takes have one.
    pub planes: u8,
}

/// One buffer off the CAPTURE queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dequeued {
    pub index: u32,
    /// The stamp of the access unit this picture decodes; drivers copy it.
    pub stamp: u64,
    /// `V4L2_BUF_FLAG_ERROR`: the picture may be corrupt.
    pub error: bool,
    /// No payload: the end-of-drain marker, or a buffer returned unused.
    pub empty: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    SourceChange,
    Other,
}

/// What [`Device::wait`] sleeps on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Interest {
    /// A decoded picture or an event.
    Capture,
    /// A returned input buffer or an event.
    Output,
}

/// The ioctl surface of one decoder node. `Ok(None)` from a dequeue is
/// `EAGAIN`: nothing ready, not a failure.
pub trait Device {
    fn set_output_format(
        &mut self,
        fourcc: u32,
        width: u32,
        height: u32,
        buffer_size: u32,
    ) -> std::io::Result<()>;
    fn subscribe_source_change(&mut self) -> std::io::Result<()>;
    /// Returns the count the driver granted. Zero frees the queue.
    fn request_buffers(&mut self, queue: Queue, count: u32) -> std::io::Result<u32>;
    fn stream(&mut self, queue: Queue, on: bool) -> std::io::Result<()>;
    fn queue_output(&mut self, index: u32, data: &[u8], stamp: u64) -> std::io::Result<()>;
    fn dequeue_output(&mut self) -> std::io::Result<Option<u32>>;
    fn queue_capture(&mut self, index: u32) -> std::io::Result<()>;
    fn dequeue_capture(&mut self) -> std::io::Result<Option<Dequeued>>;
    fn dequeue_event(&mut self) -> std::io::Result<Option<Event>>;
    fn capture_format(&mut self) -> std::io::Result<CaptureFormat>;
    fn capture_formats(&mut self) -> std::io::Result<Vec<u32>>;
    fn set_capture_format(&mut self, fourcc: u32) -> std::io::Result<CaptureFormat>;
    fn min_capture_buffers(&mut self) -> std::io::Result<u32>;
    /// Returns when something in `interest` is ready or the timeout passes.
    fn wait(&mut self, interest: Interest, timeout: Duration) -> std::io::Result<()>;
}

#[derive(Debug)]
pub enum Error {
    /// An ioctl failed. `op` names the operation.
    Device {
        op: &'static str,
        source: std::io::Error,
    },
    /// The decoder offers none of the wanted picture formats.
    NoUsableFormat { offered: Vec<u32> },
    /// The decoder stopped returning input buffers.
    Stalled,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Device { op, source } => write!(f, "{op}: {source}"),
            Error::NoUsableFormat { offered } => {
                write!(f, "no importable picture format among")?;
                for code in offered {
                    write!(f, " {}", fourcc_name(*code))?;
                }
                Ok(())
            }
            Error::Stalled => write!(f, "the decoder returned no input buffer"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Device { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// A fourcc as its four characters, for logs.
pub fn fourcc_name(code: u32) -> String {
    code.to_le_bytes()
        .iter()
        .map(|b| {
            if b.is_ascii_graphic() {
                *b as char
            } else {
                '?'
            }
        })
        .collect()
}

fn op<T>(op: &'static str, r: std::io::Result<T>) -> Result<T, Error> {
    r.map_err(|source| Error::Device { op, source })
}

/// The allocated CAPTURE queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capture {
    pub format: CaptureFormat,
    pub buffers: u32,
    /// Bumped on every reallocation. Exported fds and release tokens carry it.
    pub generation: u64,
    /// Handed to the consumer and not yet released.
    held: Vec<bool>,
}

/// One decoded picture, owned by the caller until [`Stateful::release`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Picture {
    pub index: u32,
    pub stamp: u64,
    pub generation: u64,
    /// The driver flagged the picture as possibly corrupt.
    pub corrupt: bool,
}

pub struct Stateful<D: Device> {
    dev: D,
    free_inputs: Vec<u32>,
    capture: Option<Capture>,
    generation: u64,
    /// Picture formats the caller can display, most wanted first.
    wanted: Vec<u32>,
}

impl<D: Device> Stateful<D> {
    /// Configure the OUTPUT side and start it. `width`/`height` size the input
    /// buffers and are a hint; the stream's own headers decide the picture.
    pub fn open(
        mut dev: D,
        codec: u32,
        width: u32,
        height: u32,
        wanted: &[u32],
    ) -> Result<Stateful<D>, Error> {
        // An intra frame at a high bitrate is several times the average; one
        // byte per pixel is past any rate the host sends, 2 MiB covers small modes.
        let buffer_size = width.saturating_mul(height).max(2 << 20);
        op(
            "set the input format",
            dev.set_output_format(codec, width, height, buffer_size),
        )?;
        op(
            "subscribe to source-change events",
            dev.subscribe_source_change(),
        )?;
        let granted = op(
            "allocate input buffers",
            dev.request_buffers(Queue::Output, OUTPUT_BUFFERS),
        )?;
        op("start the input queue", dev.stream(Queue::Output, true))?;
        Ok(Stateful {
            dev,
            free_inputs: (0..granted).rev().collect(),
            capture: None,
            generation: 0,
            wanted: wanted.to_vec(),
        })
    }

    pub fn device(&mut self) -> &mut D {
        &mut self.dev
    }

    /// The CAPTURE queue, once the first source change configured it.
    pub fn capture(&self) -> Option<&Capture> {
        self.capture.as_ref()
    }

    /// Queue one access unit. `stamp` comes back on the picture it decodes to.
    pub fn submit(&mut self, au: &[u8], stamp: u64) -> Result<(), Error> {
        self.reclaim_inputs()?;
        if self.free_inputs.is_empty() {
            // A decoder waiting on its CAPTURE queue returns no inputs.
            self.answer_events()?;
            op(
                "wait for an input buffer",
                self.dev.wait(Interest::Output, SUBMIT_WAIT),
            )?;
            self.reclaim_inputs()?;
        }
        let Some(index) = self.free_inputs.pop() else {
            return Err(Error::Stalled);
        };
        // A refused buffer was never queued, so no dequeue returns it: it is still ours.
        op(
            "queue an access unit",
            self.dev.queue_output(index, au, stamp),
        )
        .inspect_err(|_| self.free_inputs.push(index))
    }

    /// The next decoded picture, waiting up to `timeout` for one.
    pub fn pump(&mut self, timeout: Duration) -> Result<Option<Picture>, Error> {
        let deadline = Instant::now() + timeout;
        loop {
            // Pictures before events: answering a source change drops them.
            if let Some(picture) = self.take_picture()? {
                return Ok(Some(picture));
            }
            if self.answer_events()? {
                continue;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(None);
            }
            op("wait for a picture", self.dev.wait(Interest::Capture, left))?;
        }
    }

    /// Give a picture's buffer back to the decoder. A token from a replaced
    /// pool is dropped: its index names a different buffer now.
    pub fn release(&mut self, index: u32, generation: u64) -> Result<(), Error> {
        let Some(c) = self.capture.as_mut() else {
            return Ok(());
        };
        let Some(held) = c.held.get_mut(index as usize) else {
            return Ok(());
        };
        if c.generation != generation || !*held {
            return Ok(());
        }
        *held = false;
        op("return a picture buffer", self.dev.queue_capture(index))
    }

    fn reclaim_inputs(&mut self) -> Result<(), Error> {
        while let Some(index) = op("dequeue an input buffer", self.dev.dequeue_output())? {
            self.free_inputs.push(index);
        }
        Ok(())
    }

    fn take_picture(&mut self) -> Result<Option<Picture>, Error> {
        let Some(c) = self.capture.as_mut() else {
            return Ok(None);
        };
        while let Some(b) = op("dequeue a picture", self.dev.dequeue_capture())? {
            if b.empty {
                op("return an empty buffer", self.dev.queue_capture(b.index))?;
                continue;
            }
            if let Some(held) = c.held.get_mut(b.index as usize) {
                *held = true;
            }
            return Ok(Some(Picture {
                index: b.index,
                stamp: b.stamp,
                generation: c.generation,
                corrupt: b.error,
            }));
        }
        Ok(None)
    }

    /// Drain the event queue. True when a source change reconfigured CAPTURE.
    fn answer_events(&mut self) -> Result<bool, Error> {
        let mut changed = false;
        while let Some(event) = op("dequeue an event", self.dev.dequeue_event())? {
            if event == Event::SourceChange {
                changed = true;
            }
        }
        if changed {
            self.on_source_change()?;
        }
        Ok(changed)
    }

    fn on_source_change(&mut self) -> Result<(), Error> {
        let now = op("read the picture format", self.dev.capture_format())?;
        let needed = self.min_buffers() + CAPTURE_HEADROOM;
        let reusable = self.capture.as_ref().is_some_and(|c| {
            (c.format.width, c.format.height, c.format.fourcc)
                == (now.width, now.height, now.fourcc)
                && c.buffers >= needed
        });
        if !reusable {
            return self.allocate_capture(needed);
        }
        // Same pictures, same pool: restart the queue so the decoder resumes.
        op(
            "stop the picture queue",
            self.dev.stream(Queue::Capture, false),
        )?;
        let c = self.capture.as_ref().expect("reusable implies a pool");
        for index in 0..c.buffers {
            if !c.held[index as usize] {
                op("queue a picture buffer", self.dev.queue_capture(index))?;
            }
        }
        op(
            "start the picture queue",
            self.dev.stream(Queue::Capture, true),
        )
    }

    fn min_buffers(&mut self) -> u32 {
        self.dev
            .min_capture_buffers()
            .unwrap_or(CAPTURE_MIN_FALLBACK)
    }

    fn allocate_capture(&mut self, count: u32) -> Result<(), Error> {
        if self.capture.take().is_some() {
            op(
                "stop the picture queue",
                self.dev.stream(Queue::Capture, false),
            )?;
            op(
                "free the picture buffers",
                self.dev.request_buffers(Queue::Capture, 0),
            )?;
        }
        let offered = op("list picture formats", self.dev.capture_formats())?;
        let Some(fourcc) = self.wanted.iter().copied().find(|w| offered.contains(w)) else {
            return Err(Error::NoUsableFormat { offered });
        };
        let format = op(
            "set the picture format",
            self.dev.set_capture_format(fourcc),
        )?;
        if format.fourcc != fourcc || format.planes != 1 {
            return Err(Error::NoUsableFormat {
                offered: vec![format.fourcc],
            });
        }
        let buffers = op(
            "allocate picture buffers",
            self.dev.request_buffers(Queue::Capture, count),
        )?;
        for index in 0..buffers {
            op("queue a picture buffer", self.dev.queue_capture(index))?;
        }
        op(
            "start the picture queue",
            self.dev.stream(Queue::Capture, true),
        )?;
        self.generation += 1;
        self.capture = Some(Capture {
            format,
            buffers,
            generation: self.generation,
            held: vec![false; buffers as usize],
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::FakeDecoder as Fake;
    use crate::uapi::V4L2_PIX_FMT_H264;
    use crate::uapi::V4L2_PIX_FMT_NV12;
    use crate::uapi::V4L2_PIX_FMT_P010;

    const QC08C: u32 = 0x4338_3051;

    const NOW: Duration = Duration::ZERO;

    fn open(fake: Fake) -> Stateful<Fake> {
        Stateful::open(fake, V4L2_PIX_FMT_H264, 1920, 1080, &[V4L2_PIX_FMT_NV12]).expect("open")
    }

    #[test]
    fn the_first_picture_follows_the_first_source_change() {
        let mut d = open(Fake::new(1920, 1088, &[QC08C, V4L2_PIX_FMT_NV12]));
        assert!(d.capture().is_none());
        d.submit(b"idr", 7).unwrap();
        let p = d.pump(NOW).unwrap().expect("a picture");
        assert_eq!((p.stamp, p.generation, p.corrupt), (7, 1, false));
        let c = d.capture().expect("configured");
        assert_eq!(c.format.fourcc, V4L2_PIX_FMT_NV12);
        assert_eq!((c.format.width, c.format.height), (1920, 1088));
        // The decoder's two plus the presenter's headroom.
        assert_eq!(c.buffers, 2 + CAPTURE_HEADROOM);
    }

    #[test]
    fn each_access_unit_returns_its_own_stamp() {
        let mut d = open(Fake::new(1280, 720, &[V4L2_PIX_FMT_NV12]));
        for stamp in 0..20u64 {
            d.submit(b"au", stamp).unwrap();
            let p = d.pump(NOW).unwrap().expect("one in, one out");
            assert_eq!(p.stamp, stamp);
            d.release(p.index, p.generation).unwrap();
        }
    }

    #[test]
    fn a_held_picture_is_not_decoded_into() {
        let mut d = open(Fake::new(1280, 720, &[V4L2_PIX_FMT_NV12]));
        let mut held = Vec::new();
        for stamp in 0..6u64 {
            d.submit(b"au", stamp).unwrap();
            let p = d.pump(NOW).unwrap().expect("a free buffer remains");
            assert!(!held.contains(&p.index), "buffer {} reused", p.index);
            held.push(p.index);
        }
        // Every buffer is out: the decoder has nowhere to put the next picture.
        d.submit(b"au", 6).unwrap();
        assert_eq!(d.pump(NOW).unwrap(), None);
        d.release(held[0], 1).unwrap();
        assert_eq!(d.pump(NOW).unwrap().map(|p| p.stamp), Some(6));
    }

    #[test]
    fn a_resolution_change_reallocates_and_retires_old_tokens() {
        let mut d = open(Fake::new(1280, 720, &[V4L2_PIX_FMT_NV12]));
        d.submit(b"au", 0).unwrap();
        let old = d.pump(NOW).unwrap().unwrap();
        d.device().stream = (1920, 1088);
        d.submit(b"idr", 1).unwrap();
        let new = d
            .pump(NOW)
            .unwrap()
            .expect("decoding resumes at the new size");
        assert_eq!((new.stamp, new.generation), (1, 2));
        assert_eq!(d.capture().unwrap().format.width, 1920);
        assert!(d.device().log.ends_with(&["free", "allocate"]));
        // The old pool's token names a buffer of the new pool now; the fake
        // panics on a double queue if it were applied.
        d.release(old.index, old.generation).unwrap();
        d.release(new.index, new.generation).unwrap();
    }

    #[test]
    fn a_source_change_with_nothing_changed_keeps_the_pool() {
        let mut d = open(Fake::new(1280, 720, &[V4L2_PIX_FMT_NV12]));
        d.submit(b"au", 0).unwrap();
        let held = d.pump(NOW).unwrap().unwrap();
        // The keyframe quirk: the driver announces the format it already has.
        d.device().announced = None;
        d.submit(b"idr", 1).unwrap();
        let p = d.pump(NOW).unwrap().expect("decoding resumes");
        assert_eq!((p.stamp, p.generation), (1, 1));
        assert_ne!(
            p.index, held.index,
            "the held buffer stays out of the queue"
        );
        assert_eq!(d.device().log, ["allocate"]);
        d.release(held.index, held.generation).unwrap();
    }

    #[test]
    fn a_decoder_with_no_wanted_format_is_refused() {
        let mut d = open(Fake::new(1280, 720, &[QC08C]));
        d.submit(b"idr", 0).unwrap();
        match d.pump(NOW) {
            Err(Error::NoUsableFormat { offered }) => assert_eq!(offered, [QC08C]),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn the_wanted_order_picks_the_format() {
        let fake = Fake::new(1280, 720, &[V4L2_PIX_FMT_NV12, V4L2_PIX_FMT_P010]);
        let mut d =
            Stateful::open(fake, V4L2_PIX_FMT_H264, 1280, 720, &[V4L2_PIX_FMT_P010]).unwrap();
        d.submit(b"idr", 0).unwrap();
        d.pump(NOW).unwrap().unwrap();
        assert_eq!(d.capture().unwrap().format.fourcc, V4L2_PIX_FMT_P010);
    }

    #[test]
    fn a_frozen_decoder_stalls_instead_of_blocking() {
        let mut d = open(Fake::new(1280, 720, &[V4L2_PIX_FMT_NV12]));
        d.submit(b"idr", 0).unwrap();
        d.pump(NOW).unwrap().unwrap();
        d.device().frozen = true;
        for stamp in 1..=u64::from(OUTPUT_BUFFERS) {
            d.submit(b"au", stamp).unwrap();
        }
        assert!(matches!(d.submit(b"au", 9), Err(Error::Stalled)));
        assert_eq!(d.pump(NOW).unwrap(), None);
    }

    #[test]
    fn fourcc_names_read_as_text() {
        assert_eq!(fourcc_name(V4L2_PIX_FMT_NV12), "NV12");
        assert_eq!(fourcc_name(QC08C), "Q08C");
    }
}
