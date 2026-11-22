//! ScreenCaptureKit capture of one display: a pixel source (`docs/adr/0001`) with the `Feed`
//! role, and `CursorSource` when the client draws the pointer.
//!
//! An `SCStream` hands each frame to [`Output`] on a serial dispatch queue. A complete frame's
//! IOSurface-backed `CVPixelBuffer` is retained into a one-deep mailbox; an idle frame carries
//! no new pixels and delivers nothing, so a still desktop repeats its last frame, as direct
//! Wayland capture does. The stream's stop makes the source dead and fails `try_latest`.
//!
//! - `plan.rs`: the format table, the letterbox and the pointer mapping. No OS call.
//! - `cursor.rs`: the pointer's image and place, apart from the frames.
//! - `mod.rs`: open, the Objective-C output, the capturer.
//!
//! On hardware: `examples/mac_capture_probe.rs`, which needs the Screen Recording grant.

use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use block2::{DynBlock, RcBlock};
use dispatch2::{DispatchQoS, DispatchQueue, DispatchQueueAttr, DispatchRetained};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{define_class, msg_send, AllocAnyThread, DefinedClass};
use objc2_core_foundation::{CFArray, CFRetained, CFString, CFType};
use objc2_core_graphics::{
    kCGColorSpaceITUR_2100_PQ, kCGColorSpaceSRGB, kCGDisplayStreamYCbCrMatrix_ITU_R_709_2,
    CGDisplayBounds,
};
use objc2_core_media::{CMClock, CMSampleBuffer, CMTime, CMTimeFlags};
use objc2_core_video::{
    kCVImageBufferColorPrimariesKey, kCVImageBufferColorPrimaries_ITU_R_709_2,
    kCVImageBufferTransferFunctionKey, kCVImageBufferTransferFunction_ITU_R_709_2,
    kCVImageBufferTransferFunction_SMPTE_ST_2084_PQ, kCVImageBufferYCbCrMatrixKey,
    kCVImageBufferYCbCrMatrix_ITU_R_601_4, CVAttachmentMode, CVPixelBuffer, CVPixelBufferGetHeight,
    CVPixelBufferGetPixelFormatType, CVPixelBufferGetWidth,
};
use objc2_foundation::{
    NSArray, NSDictionary, NSError, NSNumber, NSObject, NSObjectProtocol, NSString,
};
use objc2_screen_capture_kit::{
    SCCaptureDynamicRange, SCContentFilter, SCDisplay, SCFrameStatus, SCShareableContent, SCStream,
    SCStreamConfiguration, SCStreamDelegate, SCStreamFrameInfoStatus, SCStreamOutput,
    SCStreamOutputType,
};
use pf_frame::{CapturedFrame, FramePayload, PixelBufferFrame, Provenance};

use crate::{Capturer, ColourFacts, CursorSource, DisplayCaptureOpts, Feed, FirstFrame, Paced};

mod cursor;
mod plan;
mod white;

/// The surfaces the stream draws into: the mailbox's, the encoder's, the loop's repeat, and one
/// to draw the next frame in. A surface still held when the pool runs dry costs a frame.
const QUEUE_DEPTH: isize = 4;
/// [`FirstFrame::Default`]. A started stream delivers its first complete frame at once.
const FIRST_FRAME: Duration = Duration::from_secs(5);
/// Each ScreenCaptureKit call that answers through a completion handler.
const ANSWER: Duration = Duration::from_secs(5);

/// ScreenCaptureKit capture of `opts.display_id`, scaled into the session's size. Fails as
/// [`crate::NoScreenRecording`] without the grant.
pub(crate) fn open(opts: DisplayCaptureOpts) -> Result<Box<dyn Capturer>> {
    let id = opts.display_id;
    if !crate::screen_capture_access() {
        return Err(anyhow::Error::new(crate::NoScreenRecording))
            .with_context(|| format!("capture display {id}"));
    }
    let request = plan::request(&opts.output)?;
    let display = find_display(id)?;
    let bounds = CGDisplayBounds(id);
    let points = (bounds.size.width, bounds.size.height);
    let content = plan::fit(points, (opts.width, opts.height));
    if content[2] < f64::from(opts.width) - 0.5 || content[3] < f64::from(opts.height) - 0.5 {
        tracing::info!(
            display = id,
            points = ?points,
            frame = ?(opts.width, opts.height),
            content = ?content,
            "display aspect differs from the session: letterboxed"
        );
    }
    let pointer = opts
        .metadata_cursor
        .then(|| cursor::Pointer::open(id, (opts.width, opts.height)))
        .flatten();
    if opts.metadata_cursor && pointer.is_none() {
        tracing::warn!(
            display = id,
            "no system cursor image: the frames carry the pointer"
        );
    }
    let config = configure(&opts, &request, pointer.is_none());
    // SAFETY: `display` is a live `SCDisplay`; the excluded-window list is empty.
    let filter = unsafe {
        SCContentFilter::initWithDisplay_excludingWindows(
            SCContentFilter::alloc(),
            &display,
            &NSArray::new(),
        )
    };
    let lift = (request.lift.map(white::Lift::new).transpose()).context("set up HDR capture")?;
    let shared = Arc::new(Shared {
        tag_601: request.tag_601,
        lift,
        ..Shared::default()
    });
    let output = Output::new(shared.clone());
    let attr = DispatchQueueAttr::with_qos_class(
        DispatchQueueAttr::SERIAL,
        DispatchQoS::UserInteractive,
        0,
    );
    let queue = DispatchQueue::new("punktfunk-sck-capture", Some(&attr));
    // SAFETY: the filter and configuration are live; `output` outlives the stream (the capturer
    // drops the stream first), and the stream keeps its delegate weakly.
    let stream = unsafe {
        SCStream::initWithFilter_configuration_delegate(
            SCStream::alloc(),
            &filter,
            &config,
            Some(ProtocolObject::from_ref(&*output)),
        )
    };
    // SAFETY: as above; `queue` is a serial queue the capturer keeps.
    unsafe {
        stream.addStreamOutput_type_sampleHandlerQueue_error(
            ProtocolObject::from_ref(&*output),
            SCStreamOutputType::Screen,
            Some(&queue),
        )
    }
    .map_err(|e| anyhow!("add the display stream output: {}", describe(&e)))?;
    // SAFETY: `done` stays alive until it answers or `answer` gives up on it.
    answer(|done| unsafe { stream.startCaptureWithCompletionHandler(Some(done)) })
        .with_context(|| format!("start capture of display {id}"))?;
    Ok(Box::new(ScCapturer {
        shared,
        stream,
        _output: output,
        _queue: queue,
        pointer,
        display_id: id,
        keepalive: Some(opts.keepalive),
    }))
}

/// The stream's configuration: the session's size and rate, a short queue, no audio, the
/// requested buffer and the colour that matches what the encoder signals.
fn configure(
    opts: &DisplayCaptureOpts,
    request: &plan::Request,
    shows_cursor: bool,
) -> Retained<SCStreamConfiguration> {
    let hz = opts.stream_hz.clamp(1, i32::MAX as u32) as i32;
    // SAFETY: setters on a configuration this function owns. The colour names are static
    // strings, so they outlive it as the unretained setters require.
    unsafe {
        let config = SCStreamConfiguration::new();
        config.setWidth(opts.width as usize);
        config.setHeight(opts.height as usize);
        config.setMinimumFrameInterval(CMTime::new(1, hz));
        config.setQueueDepth(QUEUE_DEPTH);
        config.setPixelFormat(request.fourcc);
        config.setPreservesAspectRatio(true);
        config.setShowsCursor(shows_cursor);
        config.setCapturesAudio(false);
        if request.pq {
            config.setCaptureDynamicRange(SCCaptureDynamicRange::HDRCanonicalDisplay);
            config.setColorSpaceName(kCGColorSpaceITUR_2100_PQ);
        } else {
            // BT.709 primaries under the sRGB curve: the desktop every other source sends.
            config.setColorSpaceName(kCGColorSpaceSRGB);
        }
        if request.matrix_709 {
            config.setColorMatrix(kCGDisplayStreamYCbCrMatrix_ITU_R_709_2);
        }
        config
    }
}

/// The shareable display with this id.
fn find_display(id: u32) -> Result<Retained<SCDisplay>> {
    let (tx, rx) = mpsc::channel();
    let done = RcBlock::new(
        move |content: *mut SCShareableContent, error: *mut NSError| {
            // SAFETY: each pointer is null or valid for the handler's duration.
            let found = match unsafe { (content.as_ref(), error.as_ref()) } {
                (Some(content), _) => {
                    // SAFETY: plain getters on live objects.
                    let found = unsafe { content.displays().iter().find(|d| d.displayID() == id) };
                    found.ok_or_else(|| format!("display {id} is not shareable"))
                }
                (None, Some(error)) => Err(describe(error)),
                (None, None) => Err("no shareable content".to_owned()),
            };
            let _ = tx.send(found);
        },
    );
    // SAFETY: `done` is retained by the call until it answers.
    unsafe {
        SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(
            false, true, &done,
        );
    }
    rx.recv_timeout(ANSWER)
        .map_err(|_| anyhow!("list the shareable displays: no answer within {ANSWER:?}"))?
        .map_err(|e| anyhow!("find display {id}: {e}"))
}

/// Run `call` with a completion handler and wait for its answer.
fn answer(call: impl FnOnce(&DynBlock<dyn Fn(*mut NSError)>)) -> Result<()> {
    let (tx, rx) = mpsc::channel();
    let done = RcBlock::new(move |error: *mut NSError| {
        // SAFETY: `error` is null or valid for the handler's duration.
        let _ = tx.send(unsafe { error.as_ref() }.map(describe));
    });
    call(&done);
    match rx.recv_timeout(ANSWER) {
        Ok(None) => Ok(()),
        Ok(Some(e)) => Err(anyhow!(e)),
        Err(_) => Err(anyhow!("no answer within {ANSWER:?}")),
    }
}

fn describe(error: &NSError) -> String {
    format!(
        "{} ({} {})",
        error.localizedDescription(),
        error.domain(),
        error.code()
    )
}

/// What the capturer and [`Output`] share: the output writes on its queue, the loop reads.
#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    /// Signalled on a frame, a stop and a nudge.
    changed: Condvar,
    /// [`plan::Request::tag_601`].
    tag_601: bool,
    /// [`plan::Request::lift`]: an HDR stream's frames pass through it.
    lift: Option<white::Lift>,
}

#[derive(Default)]
struct State {
    /// The newest complete frame; a newer one replaces it.
    frame: Option<CapturedFrame>,
    /// Why the stream stopped. The source is dead from then on.
    stopped: Option<String>,
    /// [`Feed::set_active`]`(false)`: frames are dropped.
    paused: bool,
    poked: bool,
    encoded: bool,
    /// Complete frames, the `source_seq`.
    complete: u64,
    /// Frames with no new pixels: idle (nothing changed), and blank, suspended or started.
    idle: u64,
    other: u64,
    /// The newest complete frame is BT.2100 PQ.
    pq: bool,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn update(&self, f: impl FnOnce(&mut State)) {
        f(&mut self.lock());
        self.changed.notify_all();
    }

    /// One screen sample from the stream. Runs on the output queue; must not panic.
    fn arrive(&self, sample: &CMSampleBuffer) {
        match frame_status(sample) {
            Some(SCFrameStatus::Complete) => {}
            Some(SCFrameStatus::Idle) => return self.lock().idle += 1,
            Some(SCFrameStatus::Stopped) => {
                return self.stop("the stream reported it stopped".into())
            }
            _ => return self.lock().other += 1,
        }
        // SAFETY: `sample` is the valid buffer the stream passed in.
        let Some(image) = (unsafe { sample.image_buffer() }) else {
            return;
        };
        let image = match &self.lift {
            Some(lift) => match lift.convert(&image) {
                Ok(lifted) => lifted,
                Err(e) => return self.stop(format!("{e:#}")),
            },
            None => image,
        };
        let code = CVPixelBufferGetPixelFormatType(&image);
        let Some(format) = plan::delivered(code) else {
            return self.stop(format!("unexpected pixel format {}", fourcc(code)));
        };
        if self.tag_601 {
            tag_bt601(&image);
        }
        // SAFETY: static strings.
        let (key, want) = unsafe {
            (
                kCVImageBufferTransferFunctionKey,
                kCVImageBufferTransferFunction_SMPTE_ST_2084_PQ,
            )
        };
        let pq = tag(&image, key).is_some_and(|t| *t == **want);
        // SAFETY: as above.
        let pts_host = host_ns(unsafe { sample.presentation_time_stamp() });
        let (width, height) = (
            CVPixelBufferGetWidth(&image) as u32,
            CVPixelBufferGetHeight(&image) as u32,
        );
        let pts_ns = wire_pts(pts_host);
        let mut st = self.lock();
        if st.paused || st.stopped.is_some() {
            return;
        }
        st.complete += 1;
        st.pq = pq;
        let seq = st.complete;
        let replaced = st.frame.replace(CapturedFrame {
            width,
            height,
            pts_ns,
            format,
            payload: FramePayload::PixelBuffer(PixelBufferFrame(image.clone())),
            cursor: None,
            provenance: Provenance::source(seq, pts_host.unwrap_or(0)),
        });
        drop(st);
        drop(replaced);
        self.changed.notify_all();
        if seq == 1 {
            log_first(&image);
        }
    }

    fn stop(&self, why: String) {
        tracing::warn!(reason = %why, "screencapturekit stream stopped");
        self.update(|st| {
            st.stopped.get_or_insert(why);
        });
    }

    fn wait<'a>(&'a self, st: MutexGuard<'a, State>, left: Duration) -> MutexGuard<'a, State> {
        let waited = self.changed.wait_timeout(st, left);
        waited.unwrap_or_else(|e| e.into_inner()).0
    }

    /// [`Capturer::next_frame`]: the next frame within `budget`, which a stop ends early.
    fn frame_within(&self, budget: Duration) -> std::result::Result<CapturedFrame, String> {
        let deadline = Instant::now() + budget;
        let mut st = self.lock();
        loop {
            if let Some(frame) = st.frame.take() {
                return Ok(frame);
            }
            if let Some(why) = &st.stopped {
                return Err(format!("stopped: {why}"));
            }
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                return Err(format!("no frame within {:.1}s", budget.as_secs_f32()));
            };
            st = self.wait(st, left);
        }
    }

    /// [`Capturer::try_latest`]: a stop fails it after the last frame.
    fn latest(&self) -> std::result::Result<Option<CapturedFrame>, String> {
        let mut st = self.lock();
        match (st.frame.take(), &st.stopped) {
            (None, Some(why)) => Err(format!("stopped: {why}")),
            (frame, _) => Ok(frame),
        }
    }

    /// [`Feed::wait_paced`]: a nudge ends it at any point and is taken; a frame or a stop ends
    /// it once `not_before` has passed; else `deadline` does. Never takes the frame.
    fn wait_paced(&self, not_before: Instant, deadline: Instant) -> Paced {
        let mut st = self.lock();
        loop {
            if std::mem::take(&mut st.encoded) {
                return Paced::Encoded;
            }
            if std::mem::take(&mut st.poked) {
                return Paced::Poked;
            }
            let now = Instant::now();
            let until = if now < not_before {
                not_before
            } else if st.frame.is_some() || st.stopped.is_some() {
                return Paced::Due;
            } else {
                deadline
            };
            let Some(left) = until.checked_duration_since(now) else {
                return Paced::Due;
            };
            st = self.wait(st, left);
        }
    }

    /// [`Feed::set_active`]: off drops the waiting frame and every frame until on.
    fn set_active(&self, active: bool) {
        let mut st = self.lock();
        st.paused = !active;
        if !active {
            st.frame = None;
        }
    }
}

/// What the stream delivers, once: the colour tags the encoder's signalling must match.
fn log_first(image: &CVPixelBuffer) {
    let tag = |key| {
        let t = tag(image, key);
        let name = t.as_deref().and_then(|t| t.downcast_ref::<CFString>());
        name.map_or_else(|| "none".to_owned(), |s| s.to_string())
    };
    // SAFETY: static strings.
    let (matrix, primaries, transfer) = unsafe {
        (
            tag(kCVImageBufferYCbCrMatrixKey),
            tag(kCVImageBufferColorPrimariesKey),
            tag(kCVImageBufferTransferFunctionKey),
        )
    };
    tracing::info!(
        format = %fourcc(CVPixelBufferGetPixelFormatType(image)),
        width = CVPixelBufferGetWidth(image),
        height = CVPixelBufferGetHeight(image),
        %matrix,
        %primaries,
        %transfer,
        "screencapturekit first frame"
    );
}

/// Tag a buffer the stream left untagged with what it holds: BT.601 YCbCr of BT.709 primaries,
/// under the transfer the 8-bit stream tags.
fn tag_bt601(image: &CVPixelBuffer) {
    // SAFETY: a live buffer this side retains; static keys and values. Setting attachments is
    // safe while the encoder has not yet seen the buffer.
    unsafe {
        for (key, value) in [
            (
                kCVImageBufferYCbCrMatrixKey,
                kCVImageBufferYCbCrMatrix_ITU_R_601_4,
            ),
            (
                kCVImageBufferColorPrimariesKey,
                kCVImageBufferColorPrimaries_ITU_R_709_2,
            ),
            (
                kCVImageBufferTransferFunctionKey,
                kCVImageBufferTransferFunction_ITU_R_709_2,
            ),
        ] {
            image.set_attachment(key, value, CVAttachmentMode::ShouldPropagate);
        }
    }
}

/// The buffer's attachment under `key`.
fn tag(image: &CVPixelBuffer, key: &CFString) -> Option<CFRetained<CFType>> {
    // SAFETY: a live buffer and key; a null mode pointer asks for no mode.
    unsafe { image.attachment(key, std::ptr::null_mut()) }
}

/// The sample's `SCFrameStatus`, from its frame-info attachment.
fn frame_status(sample: &CMSampleBuffer) -> Option<SCFrameStatus> {
    // SAFETY: a live sample; `false` creates no array.
    let array = unsafe { sample.sample_attachments_array(false) }?;
    // SAFETY: `CFArray` is toll-free bridged to `NSArray`, and a sample attachments array holds
    // one `CFDictionary` (bridged to `NSDictionary`) per sample.
    let array = unsafe {
        &*(&*array as *const CFArray).cast::<NSArray<NSDictionary<NSString, AnyObject>>>()
    };
    // SAFETY: a static string.
    let status = array
        .firstObject()?
        .objectForKey(unsafe { SCStreamFrameInfoStatus })?;
    let status = status.downcast::<NSNumber>().ok()?;
    Some(SCFrameStatus(status.integerValue()))
}

/// A host-clock time in nanoseconds; `None` when invalid.
fn host_ns(t: CMTime) -> Option<u64> {
    if !t.flags.contains(CMTimeFlags::Valid) || t.timescale <= 0 || t.value < 0 {
        return None;
    }
    u64::try_from(i128::from(t.value) * 1_000_000_000 / i128::from(t.timescale)).ok()
}

/// The wire stamp of a frame presented at `pts_host` on the host clock: realtime at delivery
/// less the frame's age, as Linux rebases a compositor stamp.
fn wire_pts(pts_host: Option<u64>) -> u64 {
    let delivery_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    // SAFETY: a read of Core Media's process-wide host clock.
    let now = unsafe { host_ns(CMClock::host_time_clock().time()) }.unwrap_or(0);
    let rt_minus_host = delivery_ns as i64 - now as i64;
    crate::pts_provenance::wire_pts(pts_host.map(|p| p as i64), delivery_ns, rt_minus_host).pts_ns
}

fn fourcc(code: u32) -> String {
    String::from_utf8_lossy(&code.to_be_bytes()).into_owned()
}

define_class!(
    // SAFETY: `NSObject` has no subclassing requirements, and `Output` implements no `Drop`.
    #[unsafe(super(NSObject))]
    #[ivars = Arc<Shared>]
    struct Output;

    unsafe impl NSObjectProtocol for Output {}

    unsafe impl SCStreamOutput for Output {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn did_output(
            &self,
            _stream: &SCStream,
            sample: &CMSampleBuffer,
            kind: SCStreamOutputType,
        ) {
            if kind == SCStreamOutputType::Screen {
                self.ivars().arrive(sample);
            }
        }
    }

    unsafe impl SCStreamDelegate for Output {
        #[unsafe(method(stream:didStopWithError:))]
        fn did_stop(&self, _stream: &SCStream, error: &NSError) {
            self.ivars().stop(describe(error));
        }
    }
);

impl Output {
    fn new(shared: Arc<Shared>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(shared);
        // SAFETY: `NSObject`'s designated initializer on a fresh allocation.
        unsafe { msg_send![super(this), init] }
    }
}

/// The encode loop's end of one display stream.
struct ScCapturer {
    shared: Arc<Shared>,
    stream: Retained<SCStream>,
    /// The stream's output and delegate, which it does not keep alive itself.
    _output: Retained<Output>,
    _queue: DispatchRetained<DispatchQueue>,
    pointer: Option<cursor::Pointer>,
    display_id: u32,
    /// The display's lease, dropped after the stream stops unless a rebuild took it back.
    keepalive: Option<Box<dyn Send>>,
}

// SAFETY: `SCStream` may be started and stopped from any thread; it runs its handlers on its own
// queues. `Output`'s only state is an `Arc` of `Send + Sync` cells, and the dispatch queue is
// thread-safe.
unsafe impl Send for ScCapturer {}

impl ScCapturer {
    fn nudge(&self, set: fn(&mut State)) -> Arc<dyn Fn() + Send + Sync> {
        let shared = self.shared.clone();
        Arc::new(move || shared.update(set))
    }
}

impl Capturer for ScCapturer {
    /// No offer to convict: a provisional budget is only a shorter wait.
    fn next_frame(&mut self, budget: FirstFrame) -> Result<CapturedFrame> {
        let budget = match budget {
            FirstFrame::Default => FIRST_FRAME,
            FirstFrame::Within(b) | FirstFrame::Provisional(b) => b,
        };
        let id = self.display_id;
        (self.shared.frame_within(budget)).map_err(|e| anyhow!("capture of display {id}: {e}"))
    }

    /// A stopped stream fails here, after its last frame, so the loop rebuilds.
    fn try_latest(&mut self) -> Result<Option<CapturedFrame>> {
        let id = self.display_id;
        (self.shared.latest()).map_err(|e| anyhow!("capture of display {id}: {e}"))
    }

    fn is_alive(&self) -> bool {
        self.shared.lock().stopped.is_none()
    }

    fn colour(&self) -> ColourFacts {
        // Generic HDR10 names 1000 nits: the range of an HDR virtual display, whose headroom
        // of 5 sits over the stream's 203-nit white.
        ColourFacts {
            hdr_meta: self.shared.lock().pq.then(pf_frame::hdr::generic_hdr10),
            sdr_fp16: false,
        }
    }

    fn feed(&mut self) -> Option<&mut dyn Feed> {
        Some(self)
    }

    fn cursor(&mut self) -> Option<&mut dyn CursorSource> {
        self.pointer.as_mut().map(|p| p as &mut dyn CursorSource)
    }
}

impl Feed for ScCapturer {
    fn wait_arrival(&mut self, deadline: Instant) {
        self.wait_paced(Instant::now(), deadline);
    }

    fn wait_paced(&mut self, not_before: Instant, deadline: Instant) -> Paced {
        self.shared.wait_paced(not_before, deadline)
    }

    fn arrival_poke(&self) -> Option<Arc<dyn Fn() + Send + Sync>> {
        Some(self.nudge(|st| st.poked = true))
    }

    fn au_wake(&self) -> Option<Arc<dyn Fn() + Send + Sync>> {
        Some(self.nudge(|st| st.encoded = true))
    }

    fn set_active(&mut self, active: bool) {
        self.shared.set_active(active);
    }

    fn take_keepalive(&mut self) -> Option<Box<dyn Send>> {
        self.keepalive.take()
    }
}

impl Drop for ScCapturer {
    fn drop(&mut self) {
        if self.is_alive() {
            // SAFETY: `done` stays alive until it answers or `answer` gives up on it.
            let stopped =
                answer(|done| unsafe { self.stream.stopCaptureWithCompletionHandler(Some(done)) });
            if let Err(e) = stopped {
                tracing::warn!(error = %format!("{e:#}"), "screencapturekit stream did not stop");
            }
        }
        let st = self.shared.lock();
        // What the HDR white pass took per frame, GPU wait included; 0 without one.
        let lift_us = self.shared.lift.as_ref().map_or(0, white::Lift::mean_us);
        tracing::info!(
            display = self.display_id,
            complete = st.complete,
            idle = st.idle,
            other = st.other,
            lift_us,
            "screencapturekit capture closed"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pf_frame::PixelFormat;

    fn put_frame(shared: &Shared) {
        shared.update(|st| {
            st.frame = Some(CapturedFrame {
                width: 1,
                height: 1,
                pts_ns: 1,
                format: PixelFormat::Bgra,
                payload: FramePayload::Cpu(vec![0; 4]),
                cursor: None,
                provenance: Provenance::default(),
            })
        });
    }

    /// A stop ends the first-frame wait at once and fails `try_latest`, but only after the
    /// frame that was waiting.
    #[test]
    fn a_stop_fails_the_reads_after_the_last_frame() {
        let shared = Shared::default();
        let t0 = Instant::now();
        let quiet = shared.frame_within(Duration::from_millis(50));
        assert!(quiet.err().is_some_and(|e| e.contains("no frame within")));
        assert!(t0.elapsed() >= Duration::from_millis(50));
        assert!(
            matches!(shared.latest(), Ok(None)),
            "a still display is idle"
        );
        put_frame(&shared);
        shared.stop("display gone".into());
        assert!(
            matches!(shared.latest(), Ok(Some(_))),
            "the last frame first"
        );
        assert!(shared
            .latest()
            .err()
            .is_some_and(|e| e.contains("display gone")));
        let t0 = Instant::now();
        assert!(shared.frame_within(Duration::from_secs(5)).is_err());
        assert!(t0.elapsed() < Duration::from_secs(1));
    }

    /// A nudge from another thread ends the wait with its own answer, the gate before
    /// `not_before` included, and is taken; a frame waits the gate out and stays.
    #[test]
    fn a_nudge_ends_the_gate_and_a_frame_waits_for_it() {
        let shared = Shared::default();
        let poke: fn(&mut State) = |st| st.poked = true;
        let encoded: fn(&mut State) = |st| st.encoded = true;
        for (set, answer) in [(poke, Paced::Poked), (encoded, Paced::Encoded)] {
            let t0 = Instant::now();
            let (gate, deadline) = (t0 + Duration::from_secs(2), t0 + Duration::from_secs(3));
            let woke = std::thread::scope(|s| {
                s.spawn(|| {
                    std::thread::sleep(Duration::from_millis(20));
                    shared.update(set);
                });
                shared.wait_paced(gate, deadline)
            });
            assert_eq!(woke, answer);
            assert!(t0.elapsed() < Duration::from_secs(1), "woke on {answer:?}");
        }
        put_frame(&shared);
        let t0 = Instant::now();
        let gate = t0 + Duration::from_millis(60);
        assert_eq!(
            shared.wait_paced(gate, t0 + Duration::from_secs(2)),
            Paced::Due
        );
        assert!(Instant::now() >= gate, "a frame ended the gate");
        assert!(
            matches!(shared.latest(), Ok(Some(_))),
            "the wait left the frame"
        );
    }

    /// Off drops the waiting frame, so a pooled source never hands it to the next stream.
    #[test]
    fn inactive_drops_the_waiting_frame() {
        let shared = Shared::default();
        put_frame(&shared);
        shared.set_active(false);
        assert!(matches!(shared.latest(), Ok(None)));
        assert!(shared.lock().paused);
        shared.set_active(true);
        assert!(!shared.lock().paused);
    }
}
