//! The fake kit: a far side the tests play behind a [`Transport`] — the encoder of the driver
//! or of a capture worker, publishing access units through the driver's own `HeapRing`
//! (01's `Producer`) into a leaked `Vec` section — on a virtual clock.
//!
//! [`FarSide`] records what the source asked (`SET_ENCODE`, every `ENCODE_CTL`, every reaped
//! value) and scripts what it answers (a refusal, the news `poll` hands out, the secure
//! desktop, an exit). The session tests at the end run the stream loop's sequence over it.

use std::cell::Cell;
use std::collections::VecDeque;
use std::mem::offset_of;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use bytemuck::Zeroable;
use pf_driver_proto::encode::{
    self, au, backend, EncodeCtlRequest, SetEncodeReply, SetEncodeRequest,
};
use pf_driver_proto::section::SectionView;

use super::au_assembly::Clocks;
use super::au_reader::producer::Producer;
use super::au_reader::AuView;
use super::transport::{
    AuMemory, HandleTarget, Planted, RemoteEndpoint, Transport, TransportEvent,
};
use crate::{DriverEncodeParams, EncodeCtlSender, EncoderSite};

/// One test thread's clock: an offset over a process-wide base the test advances. QPC runs at
/// 1 MHz from 1 s, so no stamp is ever `0`.
pub(crate) mod clock {
    use super::*;

    thread_local! {
        static AT: Cell<Duration> = const { Cell::new(Duration::ZERO) };
    }

    fn base() -> Instant {
        static BASE: OnceLock<Instant> = OnceLock::new();
        *BASE.get_or_init(Instant::now)
    }

    fn at() -> Duration {
        AT.with(Cell::get)
    }

    pub(crate) fn advance(by: Duration) {
        AT.with(|at| at.set(at.get() + by));
    }

    fn now() -> Instant {
        base() + at()
    }

    fn now_ns() -> u64 {
        5_000_000_000 + at().as_nanos() as u64
    }

    pub(crate) fn qpc() -> u64 {
        1_000_000 + at().as_micros() as u64
    }

    fn qpc_age_us(stamp: u64) -> u64 {
        qpc().saturating_sub(stamp)
    }

    fn qpc_ticks_us(ticks: u64) -> u64 {
        ticks
    }

    pub(crate) const CLOCKS: Clocks = Clocks {
        now,
        now_ns,
        qpc_age_us,
        qpc_ticks_us,
    };
}

/// How the next `SET_ENCODE` fails.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Refusal {
    /// The IOCTL itself failed: nothing was adopted.
    Ioctl,
    /// It completed with a short reply: the far side did adopt.
    ShortReply,
    /// It completed and opened no encoder, with this status and the `noswap` stage.
    Status(u32),
}

/// What the source asked of the far side, what the far side answers, and its encoder.
#[derive(Default)]
pub(crate) struct FarSide {
    pub set_encodes: Vec<SetEncodeRequest>,
    pub ctls: Vec<EncodeCtlRequest>,
    /// Planted values the host closed because they were never adopted.
    pub closed: Vec<u64>,
    pub refuse: Option<Refusal>,
    /// One batch per `poll`; empty answers no news.
    pub news: VecDeque<Result<Vec<TransportEvent>>>,
    pub secure: bool,
    pub exited: bool,
    /// The far side stamps `frames_input` at each open, as a v10 driver does.
    pub frames_input: bool,
    /// Every planted section by its value, as the far side maps it.
    planted: Vec<(u64, AuView<'static>)>,
    /// The open encoder over the last adopted section.
    encoder: Option<Producer<'static>>,
    pub generation: u32,
    wire_seq: u32,
    source_seq: u64,
    published: u64,
}

impl FarSide {
    /// The encoder's pool takes a composed frame: the header's source counter and the drain
    /// heartbeat move.
    pub(crate) fn compose(&mut self) {
        let Some(drv) = self.encoder.as_ref() else {
            return;
        };
        self.source_seq += 1;
        drv.stamp64(offset_of!(au::AuHeader, source_seq), self.source_seq);
        drv.stamp64(offset_of!(au::AuHeader, drain_heartbeat_qpc), clock::qpc());
    }

    /// The encoder publishes the last composed frame as one whole access unit, its bytes
    /// naming its wire index: `au<n>`.
    pub(crate) fn encode(&mut self, keyframe: bool) {
        let Some(drv) = self.encoder.as_mut() else {
            return;
        };
        let flags = au::AU_FIRST | au::AU_LAST | if keyframe { au::AU_KEYFRAME } else { 0 };
        let data = format!("au{}", self.wire_seq);
        let now = clock::qpc();
        drv.publish(
            data.as_bytes(),
            self.wire_seq,
            self.source_seq as u32,
            now,
            flags,
        );
        self.wire_seq += 1;
        self.published += 1;
        drv.stamp64(offset_of!(au::AuHeader, published_total), self.published);
        drv.stamp64(offset_of!(au::AuHeader, last_au_qpc), now);
        drv.stamp32(
            offset_of!(au::AuHeader, encoder_state),
            au::ENCODER_ENCODING,
        );
    }

    /// A composed frame and its access unit.
    pub(crate) fn frame(&mut self, keyframe: bool) {
        self.compose();
        self.encode(keyframe);
    }

    /// Whether an encoder is open, and of which generation.
    pub(crate) fn open_generation(&self) -> Option<u32> {
        self.encoder.as_ref().map(|_| self.generation)
    }

    fn set_encode(&mut self, req: &SetEncodeRequest) -> Result<SetEncodeReply> {
        self.set_encodes.push(*req);
        let mut reply = SetEncodeReply::zeroed();
        match self.refuse.take() {
            Some(Refusal::Ioctl) => bail!("the fake driver failed the IOCTL"),
            Some(Refusal::ShortReply) => {
                return Err(anyhow::Error::new(encode::ReplyTooShort {
                    got: 4,
                    want: 56,
                }))
            }
            Some(Refusal::Status(status)) => {
                reply.status = status;
                reply.name[..6].copy_from_slice(b"noswap");
                return Ok(reply);
            }
            None => {}
        }
        let Some(view) = self
            .planted
            .iter()
            .find(|(value, _)| *value == req.section)
            .map(|(_, view)| view.section())
        else {
            bail!("SET_ENCODE names no planted section");
        };
        self.generation += 1;
        let drv = Producer::attach(view, self.generation);
        if self.frames_input {
            drv.stamp32(offset_of!(au::AuHeader, frames_input), 1);
        }
        self.encoder = Some(drv);
        (self.wire_seq, self.source_seq, self.published) = (req.wire_seq_base, 0, 0);
        reply.backend_opened = backend::AMF;
        reply.applied_bitrate_kbps = req.bitrate_kbps;
        reply.name[..3].copy_from_slice(b"amf");
        Ok(reply)
    }

    /// `RESET` restarts the wire domain where the host says; `CLOSE` stops only its own
    /// generation, so a predecessor dropped late never stops its successor.
    fn ctl(&mut self, req: &EncodeCtlRequest) {
        self.ctls.push(*req);
        match req.op {
            encode::ENCODE_CTL_RESET => self.wire_seq = req.arg0,
            encode::ENCODE_CTL_CLOSE if req.arg0 == self.generation => self.encoder = None,
            _ => {}
        }
    }
}

fn lock(far: &Mutex<FarSide>) -> MutexGuard<'_, FarSide> {
    far.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A transport, and the far side the test plays through it.
pub(crate) fn transport(site: EncoderSite) -> (FakeTransport, Arc<Mutex<FarSide>>) {
    let far = Arc::new(Mutex::new(FarSide::default()));
    (
        FakeTransport {
            far: far.clone(),
            site,
        },
        far,
    )
}

/// The far side as the source reaches it.
pub(crate) struct FakeTransport {
    far: Arc<Mutex<FarSide>>,
    site: EncoderSite,
}

/// The monitor every fake session opens for.
pub(crate) const TARGET_ID: u32 = 7;

impl Transport for FakeTransport {
    fn endpoint(&self) -> RemoteEndpoint {
        RemoteEndpoint {
            site: self.site,
            target_id: TARGET_ID,
        }
    }

    fn name(&self) -> &str {
        "the fake monitor"
    }

    fn handles(&self) -> &dyn HandleTarget {
        self
    }

    fn set_encode(&self, req: &SetEncodeRequest) -> Result<SetEncodeReply> {
        lock(&self.far).set_encode(req)
    }

    fn encode_ctl(&self) -> EncodeCtlSender {
        let far = self.far.clone();
        Arc::new(move |req: &EncodeCtlRequest| {
            lock(&far).ctl(req);
            Ok(())
        })
    }

    fn secure_desktop(&self) -> bool {
        lock(&self.far).secure
    }

    fn poll(&mut self, _now: Instant, _last_fresh: Instant) -> Result<Vec<TransportEvent>> {
        lock(&self.far).news.pop_front().unwrap_or(Ok(Vec::new()))
    }

    fn alive(&self) -> bool {
        !lock(&self.far).exited
    }
}

impl HandleTarget for FakeTransport {
    /// A zeroed section the far side keeps by its value; leaked, so its view is `'static`.
    fn plant(&self, bytes: usize) -> Result<Planted> {
        let view = SectionView::over(Vec::leak(vec![0u64; bytes.div_ceil(8)]));
        let mut far = lock(&self.far);
        let section = 0x100 + 2 * far.planted.len() as u64;
        far.planted.push((section, AuView::new(view)));
        Ok(Planted {
            memory: Box::new(FakeMemory(AuView::new(view))),
            section,
            event: section + 1,
        })
    }

    fn close_remote(&self, value: u64) {
        lock(&self.far).closed.push(value);
    }
}

/// The host's view of a planted fake section. Its wait returns at once: the tests publish
/// before they read.
struct FakeMemory(AuView<'static>);

impl AuMemory for FakeMemory {
    // unsafe-fn-no-op-ok: the contract is the trait's; a leaked buffer outlives every view.
    unsafe fn share(&self) -> SectionView<'static> {
        self.0.section()
    }

    fn wait(&self, _timeout_ms: u32) {}

    fn signal(&self) -> Box<dyn Fn() + Send + Sync> {
        Box::new(|| {})
    }
}

/// What the loop asks a fake encoder to open: HEVC at `width`×`height`, 60 fps.
pub(crate) fn params(width: u32, height: u32, hdr: bool, wire_seq_base: u32) -> DriverEncodeParams {
    DriverEncodeParams {
        codec: 2,
        chroma: 0,
        bit_depth: if hdr { 10 } else { 8 },
        width,
        height,
        fps: 60,
        bitrate_kbps: 20_000,
        peak_kbps: 20_000,
        hdr,
        hdr_meta: None,
        sdr_fp16: false,
        full_range: true,
        slices: 1,
        wire_chunk_bytes: 0,
        backends: [backend::AMF, 0, 0, 0],
        wire_seq_base,
        encode_width: 0,
        encode_height: 0,
        src_rect: [0; 4],
    }
}

/// Whole sessions over the fake: the stream loop's sequence each tick — the encoder's clocks
/// in, the source's tick, then every access unit the encoder owes — through the far side's
/// resizes, colour changes, wedges and resets.
pub(crate) mod sessions {
    use super::*;
    use crate::windows::remote::source::{Geometry, Local, NoRungs, RemoteSource};
    use crate::windows::remote::supervisor::Supervision;
    use crate::{Capturer, Supervised};
    use pf_encode_core::Encoder;
    use pf_frame::recovery::{Stage, StageOutcome};
    use pf_frame::{CapturedFrame, PixelFormat};

    const STEP: Duration = Duration::from_millis(250);

    fn geometry(width: u32, height: u32, format: PixelFormat) -> Geometry {
        Geometry {
            width,
            height,
            format,
        }
    }

    /// One tick as the loop saw it: the delivered size, and each access unit read as its
    /// bytes and keyframe bit.
    type Tick = (Option<(u32, u32)>, Vec<(String, bool)>);

    /// The stream loop's side of one session.
    pub(crate) struct Loop {
        pub(crate) src: RemoteSource<FakeTransport>,
        pub(crate) far: Arc<Mutex<FarSide>>,
        enc: Option<Box<dyn Encoder>>,
        /// Wire indexes handed out so far: the next encoder's base.
        au_seq: u32,
    }

    impl Loop {
        pub(crate) fn new(site: EncoderSite, supervision: Supervision, geometry: Geometry) -> Self {
            let (transport, far) = transport(site);
            let src = RemoteSource::new(
                transport,
                geometry,
                supervision,
                clock::CLOCKS,
                Box::new(()),
            );
            Self {
                src,
                far,
                enc: None,
                au_seq: 0,
            }
        }

        pub(crate) fn far(&self) -> MutexGuard<'_, FarSide> {
            lock(&self.far)
        }

        /// One tick: `tick` stands for the source's own (`try_latest`) or the IDD wrapper's
        /// composition of the same phases.
        pub(crate) fn run(
            &mut self,
            tick: impl FnOnce(&mut RemoteSource<FakeTransport>) -> Result<Option<CapturedFrame>>,
        ) -> Result<Tick> {
            let telemetry = self.enc.as_ref().and_then(|e| e.telemetry());
            self.src.observe(telemetry);
            let frame = tick(&mut self.src)?;
            let mut aus = Vec::new();
            if let Some(enc) = self.enc.as_mut() {
                for _ in 0..enc.ready_aus(Instant::now()).unwrap_or(0) {
                    let c = enc.poll_chunk()?.expect("an owed access unit");
                    assert!(c.first && c.last, "whole access units");
                    aus.push((String::from_utf8(c.data).unwrap(), c.keyframe));
                    self.au_seq += 1;
                }
            }
            Ok((frame.map(|f| (f.width, f.height)), aus))
        }

        /// The IDD wrapper's tick, with the display's `geometry`.
        fn idd(&mut self, geometry: Geometry) -> Tick {
            self.run(|s| s.tick(Some(geometry), Local::default(), &mut NoRungs))
                .unwrap()
        }

        /// Open the encoder for `geometry` at the loop's next wire index. The old proxy drops
        /// after its replacement opened, as the loop's rebuild drops it.
        pub(crate) fn open(&mut self, geometry: Geometry) {
            let hdr = geometry.format == PixelFormat::P010;
            let params = params(geometry.width, geometry.height, hdr, self.au_seq);
            let remote = self.src.remote_encoder().expect("a remote encoder");
            let enc = remote.open(&params).unwrap();
            drop(self.enc.replace(enc));
        }

        fn class(&self) -> &'static str {
            self.src.health().class
        }

        fn events(&mut self) -> Vec<SourceEvent> {
            self.src.events()
        }
    }

    use crate::SourceEvent;

    fn au(name: &str, keyframe: bool) -> (String, bool) {
        (name.to_owned(), keyframe)
    }

    /// The driver's session under the recovery ladder, through the IDD wrapper's phases.
    #[test]
    fn an_idd_session_runs_over_the_fake_driver() {
        let hd = geometry(1920, 1080, PixelFormat::Nv12);
        let mut s = Loop::new(EncoderSite::Driver, Supervision::Ladder, hd);
        // Before any encoder the loop gets a frame to open one; the open plants one section.
        assert_eq!(s.idd(hd).0, Some((1920, 1080)));
        s.open(hd);
        let req = s.far().set_encodes.clone();
        assert_eq!(req.len(), 1);
        assert_eq!(
            (req[0].target_id, req[0].section, req[0].event),
            (TARGET_ID, 0x100, 0x101)
        );
        assert!(
            s.far().closed.is_empty(),
            "an adopted section is the far side's"
        );
        assert_eq!(s.class(), "healthy");

        // A composed frame and its access unit: one delivery, one AU owed.
        s.far().frame(true);
        assert_eq!(s.idd(hd), (Some((1920, 1080)), vec![au("au0", true)]));

        // A still desktop: nothing delivered, idle past the suspect floor, no event.
        for _ in 0..12 {
            clock::advance(STEP);
            assert_eq!(s.idd(hd), (None, vec![]));
        }
        assert_eq!(s.class(), "idle");
        assert!(s.events().is_empty());

        // A resize over the still desktop delivers once. The re-open's late CLOSE names the
        // old generation and stops nothing; the new section's 0 is no news until its pool
        // takes a frame.
        let qhd = geometry(2560, 1440, PixelFormat::Nv12);
        assert_eq!(s.idd(qhd).0, Some((2560, 1440)));
        assert_eq!(s.idd(qhd).0, None, "delivered once");
        s.open(qhd);
        let close = s.far().ctls.last().copied().unwrap();
        assert_eq!((close.op, close.arg0), (encode::ENCODE_CTL_CLOSE, 1));
        assert_eq!(s.far().open_generation(), Some(2));
        assert_eq!(s.idd(qhd).0, None, "a fresh section is no news");
        s.far().frame(false);
        assert_eq!(s.idd(qhd), (Some((2560, 1440)), vec![au("au1", false)]));

        // An HDR flip: the format change delivers, the colour follows, the re-open asks HDR.
        let hdr = geometry(2560, 1440, PixelFormat::P010);
        assert!(s.idd(hdr).0.is_some());
        assert!(s.src.colour().hdr_meta.is_some());
        s.open(hdr);
        assert_eq!(s.far().set_encodes.last().map(|r| r.hdr), Some(1));
        s.far().frame(true);
        assert_eq!(s.idd(hdr).1, [au("au2", true)]);

        // A wedge: frames keep coming and no access unit does, until the ladder asks the loop
        // for an encoder reset.
        let mut asked = None;
        for i in 0..80 {
            clock::advance(STEP);
            s.far().compose();
            let (f, aus) = s.idd(hdr);
            assert!(f.is_some() && aus.is_empty(), "tick {i}");
            if let [SourceEvent::RunStage(stage)] = s.events()[..] {
                asked = Some((i, stage));
                break;
            }
        }
        let (at, stage) = asked.expect("the ladder never asked for a reset");
        assert_eq!(stage, Stage::EncoderReset);
        assert!(at >= 59, "asked after {at} ticks, before the 15 s floor");
        let health = s.src.health();
        assert_eq!(
            (health.class, health.stall_class),
            ("stalled", Some("encoder"))
        );

        // The loop's RESET restarts the domain where the reader stands; three access units
        // prove the episode, and the loop is told the outage.
        assert!(s.enc.as_mut().unwrap().reset());
        let reset = s.far().ctls.last().copied().unwrap();
        assert_eq!((reset.op, reset.arg0), (encode::ENCODE_CTL_RESET, 3));
        s.src.stage_done(Stage::EncoderReset, StageOutcome::Applied);
        let mut read = Vec::new();
        for _ in 0..3 {
            clock::advance(STEP);
            s.far().frame(false);
            read.extend(s.idd(hdr).1);
        }
        assert_eq!(read, [au("au3", false), au("au4", false), au("au5", false)]);
        assert_eq!(
            s.events(),
            [SourceEvent::RecoveredOutage(Duration::from_millis(750))]
        );
        let episode = s.src.health().last_episode.expect("the closed episode");
        assert!(episode.recovered);
        assert_eq!(
            (episode.stages[0].0, episode.stages[0].1),
            ("encoder_reset", "applied")
        );
    }

    /// The worker's session, classify-only, through the source's own `try_latest`: a stalled
    /// encoder or a quiet mirror never raises a rung, and the worker's exit ends the source.
    #[test]
    fn a_worker_session_runs_over_the_same_fake() {
        let hd = geometry(1920, 1080, PixelFormat::Nv12);
        let classify = Supervision::Classify {
            idle_after: Duration::from_secs(2),
        };
        let mut s = Loop::new(EncoderSite::Worker, classify, hd);
        let latest = |s: &mut RemoteSource<FakeTransport>| s.try_latest();
        // The first frame within the source's own wait; the open plants one section.
        let first = s.src.next_frame(crate::FirstFrame::Default).unwrap();
        assert_eq!((first.width, first.height), (1920, 1080));
        assert_eq!(
            s.src.remote_encoder().map(|r| r.site()),
            Some(EncoderSite::Worker)
        );
        s.open(hd);
        assert_eq!(s.far().set_encodes.len(), 1);
        s.far().frame(true);
        assert_eq!(
            s.run(latest).unwrap(),
            (Some((1920, 1080)), vec![au("au0", true)])
        );

        // Frames flow and the encoder publishes nothing for 20 s: the ladder's encoder stall,
        // which classify-only never names.
        for _ in 0..80 {
            clock::advance(STEP);
            s.far().compose();
            assert_eq!(s.run(latest).unwrap(), (Some((1920, 1080)), vec![]));
            assert_eq!(s.class(), "healthy");
            assert!(s.events().is_empty());
        }
        // A still desktop: healthy under 2 s, idle past it, never stalled.
        let mut classes = Vec::new();
        for _ in 0..16 {
            clock::advance(STEP);
            assert_eq!(s.run(latest).unwrap(), (None, vec![]));
            classes.push(s.class());
        }
        assert_eq!(classes[..7], ["healthy"; 7]);
        assert_eq!(classes[7..], ["idle"; 9]);
        assert!(s.events().is_empty());

        // The worker reports a new size: it is delivered over the still desktop, and the
        // re-opened encoder's fresh section is no news until its pool takes a frame.
        let resized = TransportEvent::Resized {
            width: 2560,
            height: 1440,
        };
        s.far().news.push_back(Ok(vec![resized]));
        assert_eq!(s.run(latest).unwrap().0, Some((2560, 1440)));
        s.open(geometry(2560, 1440, PixelFormat::Nv12));
        assert_eq!(s.run(latest).unwrap().0, None);
        assert!(s.src.colour().hdr_meta.is_none());

        // The secure desktop is named first.
        s.far().secure = true;
        clock::advance(STEP);
        s.run(latest).unwrap();
        assert_eq!(s.class(), "secure_desktop");
        s.far().secure = false;

        // A loop-side reset keeps the wire order and opens no episode.
        assert!(s.enc.as_mut().unwrap().reset());
        let reset = s.far().ctls.last().copied().unwrap();
        assert_eq!((reset.op, reset.arg0), (encode::ENCODE_CTL_RESET, 1));
        let mut read = Vec::new();
        for _ in 0..2 {
            clock::advance(STEP);
            s.far().frame(false);
            read.extend(s.run(latest).unwrap().1);
        }
        assert_eq!(read, [au("au1", false), au("au2", false)]);
        let health = s.src.health();
        assert!(health.current_stage.is_none() && health.last_episode.is_none());
        assert!(s.events().is_empty());

        // The worker exits: the source says so between ticks and ends at the next one.
        s.far().exited = true;
        let exited = anyhow::anyhow!("capture worker exited (code 1)");
        s.far().news.push_back(Err(exited));
        assert!(!s.src.is_alive());
        let err = s.run(latest).unwrap_err();
        assert!(format!("{err:#}").contains("capture worker exited (code 1)"));
    }
}
