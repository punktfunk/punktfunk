//! The one pipeline swap. A stream replaces its encoder, and often its capture with it, on a
//! session switch, a mode switch, a topology re-assert, a capture loss, a source that changed
//! size, a bitrate the encoder cannot take in place, and a codec ask. The rebuild performer
//! (`StreamState::rebuild`, `rebuild.rs`) builds the replacement and is the one caller of
//! [`StreamState::swap_pipeline`], with the plan's [`SwapReason`].
//!
//! The swap owns what restarts with an encoder: the stream epoch and telling the client of it,
//! the in-flight set, the watchdog, the IDR clock, the pipelined escalation, the encoder
//! ceiling, the retarget state, the first screen's share, `enc_src`, and the display lease the
//! new pipeline replaced. What a reason leaves alone is a method of [`SwapReason`] and is
//! decided nowhere else. The plan's epilogue keeps the mode, the rate and any correction owed
//! the client.
//!
//! Pinned by `each_swap_restarts_what_its_reason_says`, one row per site.

use super::pipeline::Pipeline;
use super::state::StreamState;

/// Why the pipeline is swapped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SwapReason {
    /// Gaming↔Desktop: a new display on another backend.
    SessionSwitch,
    /// The client's accepted mode.
    ModeSwitch,
    /// An exclusive-topology eviction bounced the display's modes.
    TopologyReassert,
    CaptureLost,
    /// The source changed format or size under the live capturer.
    SourceMode,
    /// A rate the encoder cannot take in place.
    Bitrate,
    /// The client's codec ask: another codec, and with it often another depth or colour.
    Format,
}

impl SwapReason {
    /// A rate rebuild delivers the same picture at the same mode, so it keeps the epoch and
    /// the client keeps what it holds.
    fn moves_epoch(self) -> bool {
        self != SwapReason::Bitrate
    }

    /// A new geometry or a new codec, whatever the frames say: the encoder's limits are
    /// unknown again.
    fn limits_unknown(self) -> bool {
        matches!(
            self,
            SwapReason::ModeSwitch | SwapReason::SourceMode | SwapReason::Format
        )
    }

    /// The new encoder has been handed no rate and no FEC ask: `on_fec_moved` asks again. A
    /// rate rebuild opened on the rate it was asked for, which its site then records.
    fn forgets_retarget(self) -> bool {
        self != SwapReason::Bitrate
    }

    /// The display generation to release once the new pipeline is up: the one it replaced.
    /// A lease drop looks like a disconnect to keep-alive, so a replaced display would
    /// linger. A session switch's old display is another backend's, and the session watcher
    /// reaps those.
    pub(super) fn retires(self, old: Option<u64>, live: Option<u64>) -> Option<u64> {
        old.filter(|g| self != SwapReason::SessionSwitch && live != Some(*g))
    }

    /// `enc_src` follows the frame where the site publishes the delivered mode itself, or
    /// rebuilds at the mode it had. A session switch and a capture loss can land on a backend
    /// that delivers another size: left stale, `follow_source_mode` sees it, sets `cur_mode`,
    /// re-resolves the rate and tells the client. A rate rebuild leaves a source change it did
    /// not make to it too.
    fn rereads_enc_src(self) -> bool {
        match self {
            SwapReason::ModeSwitch
            | SwapReason::TopologyReassert
            | SwapReason::SourceMode
            | SwapReason::Format => true,
            SwapReason::SessionSwitch | SwapReason::CaptureLost | SwapReason::Bitrate => false,
        }
    }
}

/// What a swap installs.
pub(super) enum Next {
    /// Capture and encoder both, from a build or a re-attach.
    Pipeline(Pipeline),
    /// The encoder alone, under a capturer that keeps its attachment. `resized` is the first
    /// frame at a new size and its pacing interval, after the Windows in-place mode set.
    Encoder {
        enc: Box<dyn crate::encode::Encoder>,
        reframe: punktfunk_core::video_fit::Reframe,
        resized: Option<(crate::capture::CapturedFrame, std::time::Duration)>,
    },
}

/// What an encoder is opened for: a change in any of the three is another encoder.
fn enc_src_of(frame: &crate::capture::CapturedFrame) -> (pf_frame::PixelFormat, u32, u32) {
    (frame.format, frame.width, frame.height)
}

impl StreamState {
    /// Install `next` and restart what restarts with an encoder. The caller closed any open
    /// wire frame first ([`Self::close_open_frame`]); `next`'s encoder numbers its AUs after it.
    pub(super) fn swap_pipeline(&mut self, next: Next, why: SwapReason) {
        let old_gen = self.live.cur_display_gen;
        if why.moves_epoch() {
            self.live.epoch = self.live.epoch.wrapping_add(1);
        }
        // A ceiling was learned from the encoder this one replaces. It survives a swap that
        // opens on the same source.
        let opens_on = match &next {
            Next::Pipeline(p) => Some(&p.frame),
            Next::Encoder { resized, .. } => resized.as_ref().map(|(frame, _)| frame),
        };
        let source_moved = opens_on.is_some_and(|f| enc_src_of(f) != enc_src_of(&self.live.frame));
        if source_moved || why.limits_unknown() {
            self.shared
                .encoder_ceiling
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clear();
        }
        if why.forgets_retarget() {
            self.live.retargeted = false;
            self.fec.pending = None;
        }
        match next {
            Next::Pipeline(p) => self.adopt_pipeline(p),
            Next::Encoder {
                enc,
                reframe,
                resized,
            } => {
                self.adopt_reframe(reframe);
                self.live.enc = enc;
                self.carry_pipelining();
                if let Some((frame, interval)) = resized {
                    self.live.frame = frame;
                    self.live.interval = interval;
                }
            }
        }
        self.clock.inflight.clear();
        self.clock.watchdog.on_au();
        if let Some(g) = why.retires(old_gen, self.live.cur_display_gen) {
            crate::vdisplay::registry::retire(g);
        }
        if why.rereads_enc_src() {
            self.live.enc_src = enc_src_of(&self.live.frame);
        }
        self.live.applied_share = self.enc_now().share_permille;
        // The new encoder opened on an IDR: the keyframe cooldown counts from it, and a host
        // repair it covers is owed no longer.
        self.clock.last_forced_idr = Some(std::time::Instant::now());
        self.owed_idr = false;
        // Every epoch is told, before its first frame encodes; a site that corrects the mode
        // tells again.
        if why.moves_epoch() {
            self.tell_delivered(self.delivered_mode(), false);
        }
    }

    /// The capture side of a built pipeline, then its encoder. A new capturer numbers its
    /// cursor shapes from 1 again, so the last forwarded one is forgotten, and the clipboard
    /// follows the new capture's gamescope.
    fn adopt_pipeline(&mut self, p: Pipeline) {
        let Pipeline {
            capturer,
            enc,
            frame,
            interval,
            node_id,
            display_gen,
            bitrate_kbps: _,
            reframe,
            #[cfg(target_os = "linux")]
            lease,
            #[cfg(target_os = "linux")]
            panel_tick_ns,
        } = p;
        self.adopt_reframe(reframe);
        // The old capturer goes before the old encoder, as on teardown.
        self.live.capturer = capturer;
        self.live.asks = Default::default();
        if let Some(fwd) = self.display.cursor_fwd.as_mut() {
            *fwd = super::super::cursor_fwd::CursorForwarder::new();
        }
        self.live.enc = enc;
        self.carry_pipelining();
        self.live.frame = frame;
        self.live.interval = interval;
        self.live.cur_node_id = node_id;
        self.live.cur_display_gen = display_gen;
        #[cfg(target_os = "linux")]
        {
            super::state::publish_gamescope_xwayland(
                &self.shared.gamescope_xwayland,
                lease.as_ref(),
            );
            self.live.lease = lease;
            self.live.restamp = panel_tick_ns.map(super::encode::SourceStamps::new);
        }
        self.publish_arrival_poke();
    }

    /// The running capturer's arrival poke, for the control task. A stale handle after a
    /// swap only pokes a capturer that is gone.
    pub(super) fn publish_arrival_poke(&mut self) {
        let poke = self.live.capturer.feed().and_then(|f| f.arrival_poke());
        *self
            .shared
            .arrival_poke
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = poke;
    }

    /// A fresh encoder retrieves synchronously. The session's pipelined escalation carries onto
    /// it, or the flags claim a mode the encoder is not in and escalation never fires again.
    fn carry_pipelining(&mut self) {
        if self.pipelining.deescalating {
            // The wind-back the old encoder owed: the new one is already there.
            self.pipelining.deescalating = false;
            self.pipelining.pipelined_active = false;
            self.pipelining.pipeline_asked = false;
        } else if self.pipelining.pipelined_active {
            self.pipelining.pipelined_active = self.live.enc.set_pipelined(true);
        } else {
            // A refusal was the old encoder's; this one may say yes.
            self.pipelining.pipeline_asked = false;
        }
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::super::facts::{LoopEnds, SessionFacts, Shared};
    use super::super::state::{AuClock, Diag, DisplayRequest, FecState, Live, Pipelining};
    use super::*;
    use crate::capture::{CapturedFrame, Capturer, SyntheticCapturer};
    use crate::native::bitrate::EncoderCeiling;
    use punktfunk_core::quic::AckReason;
    use std::sync::atomic::{AtomicU32, AtomicU64, AtomicU8};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    pub(in crate::native::stream) const SMALL: (u32, u32) = (16, 16);
    pub(in crate::native::stream) const WIDE: (u32, u32) = (32, 16);
    const INTERVAL: Duration = Duration::from_millis(16);

    /// Encodes nothing.
    pub(in crate::native::stream) struct NullEnc;

    impl crate::encode::Encoder for NullEnc {
        fn submit(&mut self, _frame: &CapturedFrame) -> anyhow::Result<()> {
            Ok(())
        }
        fn poll(&mut self) -> anyhow::Result<Option<crate::encode::EncodedFrame>> {
            Ok(None)
        }
        fn flush(&mut self) -> anyhow::Result<()> {
            Ok(())
        }
    }

    /// An encoder that counts the keyframes it is asked for.
    pub(in crate::native::stream) struct CountsKeyframes(
        pub(in crate::native::stream) Arc<AtomicU32>,
    );

    impl crate::encode::Encoder for CountsKeyframes {
        fn submit(&mut self, _frame: &CapturedFrame) -> anyhow::Result<()> {
            Ok(())
        }
        fn poll(&mut self) -> anyhow::Result<Option<crate::encode::EncodedFrame>> {
            Ok(None)
        }
        fn flush(&mut self) -> anyhow::Result<()> {
            Ok(())
        }
        fn request_keyframe(&mut self) {
            self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// What the fake's display was asked: the HDR and cursor it was last told, and how many
    /// displays.
    #[derive(Default)]
    pub(in crate::native::stream) struct DisplayAsks {
        pub(in crate::native::stream) hdr: Option<bool>,
        pub(in crate::native::stream) hw_cursor: Option<bool>,
        pub(in crate::native::stream) creates: u32,
    }

    /// A display that never comes up: a capture would attach to one next, which no test box
    /// can. Each create takes [`CREATE_TAKES`], long enough to announce as a gap.
    struct NullDisplay(Arc<Mutex<DisplayAsks>>);

    const CREATE_TAKES: Duration = Duration::from_millis(3);

    impl crate::vdisplay::VirtualDisplay for NullDisplay {
        fn name(&self) -> &'static str {
            "null"
        }
        fn create(
            &mut self,
            _mode: pf_vdisplay::Mode,
        ) -> anyhow::Result<crate::vdisplay::VirtualOutput> {
            self.0.lock().unwrap().creates += 1;
            std::thread::sleep(CREATE_TAKES);
            anyhow::bail!("the test display never comes up")
        }
        fn set_hdr(&mut self, on: bool) {
            self.0.lock().unwrap().hdr = Some(on);
        }
        fn set_hw_cursor(&mut self, on: bool) {
            self.0.lock().unwrap().hw_cursor = Some(on);
        }
    }

    pub(in crate::native::stream) fn frame((w, h): (u32, u32)) -> CapturedFrame {
        SyntheticCapturer::new(w, h, 60)
            .next_frame(pf_capture::FirstFrame::Default)
            .expect("a synthetic frame")
    }

    /// A built pipeline of `size` on display generation 2.
    fn pipeline(size: (u32, u32)) -> Next {
        Next::Pipeline(Pipeline {
            capturer: Box::new(SyntheticCapturer::new(size.0, size.1, 60)),
            enc: Box::new(NullEnc),
            frame: frame(size),
            interval: INTERVAL,
            node_id: 2,
            display_gen: Some(2),
            bitrate_kbps: 20_000,
            reframe: Default::default(),
            #[cfg(target_os = "linux")]
            lease: None,
            #[cfg(target_os = "linux")]
            panel_tick_ns: None,
        })
    }

    /// An encoder alone, with the first frame of `resized` when the display was resized in place.
    fn encoder(resized: Option<(u32, u32)>) -> Next {
        Next::Encoder {
            enc: Box::new(NullEnc),
            reframe: Default::default(),
            resized: resized.map(|size| (frame(size), INTERVAL)),
        }
    }

    /// A stream loop's state with nothing behind it, and what keeps its link valid. Field
    /// order is drop order: the state goes before the runtime its connection lives on.
    pub(in crate::native::stream) struct Fake {
        pub(in crate::native::stream) st: StreamState,
        /// What the swap told the control task.
        pub(in crate::native::stream) told:
            tokio::sync::mpsc::UnboundedReceiver<crate::native::wiring::Delivered>,
        /// The gaps, codec outcomes and rate corrections the loop sent the control task.
        pub(in crate::native::stream) gaps: tokio::sync::mpsc::UnboundedReceiver<u32>,
        pub(in crate::native::stream) codecs:
            tokio::sync::mpsc::UnboundedReceiver<crate::native::wiring::FormatOutcome>,
        pub(in crate::native::stream) rates: tokio::sync::mpsc::UnboundedReceiver<(u32, AckReason)>,
        pub(in crate::native::stream) display: Arc<Mutex<DisplayAsks>>,
        _client: quinn::Connection,
        _endpoints: (quinn::Endpoint, quinn::Endpoint),
        _rt: tokio::runtime::Runtime,
        /// Last, so the session's summary lands before another test may stop every session.
        _registry: tokio::sync::MutexGuard<'static, ()>,
    }

    /// A session mid-stream on display generation 1 at [`SMALL`], with everything a swap
    /// restarts left dirty: two AUs in flight, a spent watchdog, no IDR stamp, a retarget and
    /// a FEC ask pending, a learned ceiling, and a pipelined retrieve being wound back. Its
    /// session is a live registry entry, so the fake holds the registry's test lock.
    pub(in crate::native::stream) fn fake() -> Fake {
        use punktfunk_core::quic::endpoint;
        let registry = crate::session_status::tests::registry_lock();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime");
        let (server, client, host_conn, client_conn) = rt.block_on(async {
            let server = endpoint::server("127.0.0.1:0".parse().unwrap()).expect("server");
            let addr = server.local_addr().expect("server address");
            let client = endpoint::client_insecure().expect("client");
            let (host_conn, client_conn) = tokio::join!(
                async { server.accept().await.expect("incoming").await },
                async { client.connect(addr, "punktfunk").expect("dial").await },
            );
            (
                server,
                client,
                host_conn.expect("host side connects"),
                client_conn.expect("client side connects"),
            )
        });
        let conn = crate::native::link::SessionLink::QuicV2(
            host_conn.clone(),
            Arc::new(crate::native::link::V2Link::new(
                host_conn,
                Arc::new(std::net::UdpSocket::bind("127.0.0.1:0").expect("media socket")),
            )),
        );
        let mode = punktfunk_core::Mode {
            width: SMALL.0,
            height: SMALL.1,
            refresh_hz: 60,
        };
        let now = Instant::now();
        let life = crate::lifecycle::Lifecycle::new();
        let quit = life.quit_flag().clone();
        let live = frame(SMALL);
        let mut ceiling = EncoderCeiling::new();
        ceiling.note_applied(50_000, 30_000);
        let mut watchdog = crate::encode_recovery::EncoderWatchdog::new();
        watchdog.spend(INTERVAL);
        let (frame_tx, _) = std::sync::mpsc::sync_channel(1);
        let (told_tx, told) = tokio::sync::mpsc::unbounded_channel();
        let (gap_tx, gaps) = tokio::sync::mpsc::unbounded_channel();
        let (codec_tx, codecs) = tokio::sync::mpsc::unbounded_channel();
        let (rate_tx, rates) = tokio::sync::mpsc::unbounded_channel();
        let display = Arc::new(Mutex::new(DisplayAsks::default()));
        let st = StreamState {
            facts: SessionFacts {
                deadline: now + Duration::from_secs(60),
                encoder: crate::encode::resolve(),
                knobs: crate::native::SessionKnobs::read(),
                life: life.clone(),
                counters: Default::default(),
                conn,
                negotiated: mode,
                bitrate_auto: false,
                audio_reserved_kbps: 0,
                shard_payload: 1200,
                pyrowave: false,
                streamed_wire: false,
                perf: false,
                client_hdr: None,
                bringup: crate::bringup::Trace::start("swap-test", Arc::new(AtomicU32::new(0))),
                stats: crate::stats_recorder::StatsRecorder::new(
                    std::env::temp_dir().join(format!("pf-swap-test-{}", std::process::id())),
                ),
                format_facts: crate::native::handshake::FormatFacts {
                    video_caps: 0,
                    compositor: None,
                    gamescope_route: None,
                    sdr10_widen: false,
                    studio_asked: crate::session_plan::StudioAsk::default(),
                },
            },
            next: now,
            sent: 0,
            pace: crate::send_pacing::CaptureCredit::new(now),
            clock: AuClock {
                au_seq: 40,
                wire_frame_open: false,
                inflight: [(1, 1), (2, 2)].into(),
                watchdog,
                last_forced_idr: None,
            },
            capture_rebuilds: 0,
            seen_reassert_gen: 0,
            last_hdr_meta: None,
            hdr_meta_owed: false,
            diag: Diag::new(now),
            display: DisplayRequest {
                vd: Box::new(NullDisplay(display.clone())),
                compositor: crate::vdisplay::Compositor::Kwin,
                cursor_fwd: None,
                cursor_client_drew: true,
                gamescope_composite: false,
                metadata_composite: false,
                #[cfg(target_os = "linux")]
                no_overlay_means_off_output: false,
                vd_params: crate::vdisplay::SessionParams {
                    client_fp: None,
                    client_hdr: None,
                    hdr: false,
                    hw_cursor: false,
                    join_live: false,
                    quit,
                    launch: None,
                    route: None,
                    isolation: None,
                },
                #[cfg(target_os = "linux")]
                parked_display: None,
                #[cfg(target_os = "linux")]
                park_attempts: 0,
                #[cfg(target_os = "linux")]
                next_park_at: now,
                #[cfg(target_os = "linux")]
                moves_at_arm: 0,
            },
            last_rfi: None,
            owed_idr: false,
            loss_mode: Default::default(),
            kf_gate: Default::default(),
            recovery_cadence: pf_frame::metronome::Metronome::new(),
            ir_wave_pos: 0,
            pipelining: Pipelining {
                adaptive: true,
                behind_score: 0,
                depth_frames: 0,
                pipeline_asked: true,
                pipelined_active: true,
                deescalating: true,
                ahead_run: 0,
                deescalate_not_before: None,
                deescalate_backoff: Duration::from_secs(60),
            },
            fec: FecState {
                requested: Arc::new(AtomicU8::new(20)),
                target: Arc::new(AtomicU8::new(10)),
                last: 10,
                pending: Some((20, 15_000)),
                hold_logged: 0,
            },
            src_period_ns: None,
            encode_chain_ns: 0,
            last_real_cap: None,
            _watcher: None,
            #[cfg(target_os = "linux")]
            seat_join: None,
            #[cfg(target_os = "linux")]
            following: None,
            live_session: crate::session_status::register(
                crate::session_status::Registration::fake("swap-test"),
            ),
            screens: None,
            send: super::super::send::SendThread::new(
                Arc::new(frame_tx),
                std::thread::spawn(|| {}),
            ),
            health_published_at: now,
            game_life: None,
            live: Live {
                capturer: Box::new(SyntheticCapturer::new(SMALL.0, SMALL.1, 60)),
                enc: Box::new(NullEnc),
                enc_src: enc_src_of(&live),
                frame: live,
                interval: INTERVAL,
                cur_node_id: 1,
                cur_display_gen: Some(1),
                epoch: 7,
                #[cfg(target_os = "linux")]
                lease: None,
                #[cfg(target_os = "linux")]
                restamp: None,
                cur_mode: mode,
                bitrate_kbps: 20_000,
                retargeted: true,
                asks: Default::default(),
                applied_share: crate::native::bitrate::SHARE_ALL,
            },
            plan: crate::session_plan::SessionPlan {
                capture: crate::session_plan::CaptureBackend::Portal,
                topology: crate::session_plan::SessionTopology::SingleProcess,
                encoder: crate::session_plan::EncoderBackend::Software,
                bit_depth: 8,
                hdr: false,
                chroma: crate::encode::ChromaFormat::Yuv420,
                codec: crate::encode::Codec::H265,
                wire_chunk: None,
                pyrowave_bpp: 0.0,
                cursor_blend: false,
                cursor_forward: false,
                gamescope_cursor: false,
                sdr10_native: false,
                slices: 1,
                reframe_to: None,
                rgb_full_range: true,
                studio_asked: crate::session_plan::StudioAsk::default(),
            },
            bit_depth: 8,
            shared: Shared {
                acked: Default::default(),
                lossy_window: Default::default(),
                arrival_poke: Default::default(),
                driver_dropped: Default::default(),
                screen_wire: Arc::new(crate::native::wiring::ScreenWire::new()),
                send_spread_us: Default::default(),
                wire_rekeys: Default::default(),
                live_mode: Arc::new(AtomicU64::new(0)),
                force_idr: Default::default(),
                capture_health: Default::default(),
                frame_map: Default::default(),
                #[cfg(target_os = "linux")]
                gamescope_xwayland: Default::default(),
                resize_ms: Default::default(),
                live_bitrate: Arc::new(AtomicU32::new(20_000)),
                encoder_ceiling: Arc::new(Mutex::new(ceiling)),
                cadence_degraded: Default::default(),
                cadence_behind_score: Default::default(),
                client_packets_received: Default::default(),
                cursor_client_draws: Default::default(),
            },
            ends: LoopEnds {
                cursor_shape_tx: tokio::sync::watch::channel(None).0,
                #[cfg(target_os = "linux")]
                input_tx: std::sync::mpsc::sync_channel(1).0,
                #[cfg(target_os = "linux")]
                input_route: crate::native::input::InputRoute::new(std::sync::mpsc::channel().0),
                #[cfg(target_os = "linux")]
                inj_shared_tx: std::sync::mpsc::channel().0,
                #[cfg(target_os = "linux")]
                inj_session_tx: None,
                reconfig: std::sync::mpsc::channel().1,
                keyframe: std::sync::mpsc::channel().1,
                rfi: std::sync::mpsc::channel().1,
                bitrate_rx: std::sync::mpsc::channel().1,
                session_rx: std::sync::mpsc::channel().1,
                reconfig_result_tx: told_tx,
                retarget_tx: rate_tx,
                gap_tx,
                format_rx: std::sync::mpsc::channel().1,
                format_result_tx: codec_tx,
            },
        };
        Fake {
            st,
            told,
            gaps,
            codecs,
            rates,
            display,
            _client: client_conn,
            _endpoints: (server, client),
            _rt: rt,
            _registry: registry,
        }
    }

    /// A `?` out of the loop reaches the same finish as the clean tail: the session is ended
    /// as `host_error`, and its summary carries the tally.
    #[test]
    fn an_error_exit_finishes_like_a_clean_one() {
        use crate::events::SessionEndReason;
        let Fake {
            st,
            _client,
            _endpoints,
            _rt,
            _registry,
            ..
        } = fake();
        let id = st.live_session.id;
        let life = st.facts.life.clone();
        st.finish(false);
        assert!(life.stopping());
        assert_eq!(life.reason(), Some(SessionEndReason::HostError));
        let mine = crate::session_status::recent()
            .into_iter()
            .find(|s| s.id == id)
            .expect("the finished session is summarised");
        assert_eq!(mine.ended, SessionEndReason::HostError);
        assert_eq!(mine.frames_sent, Some(0));
    }

    /// Set on the child only: its scratch config dir.
    const SCRATCH_CONFIG: &str = "PF_SWAP_TEST_CONFIG";

    /// A rebuild after the console moves the Encoder row opens on the resolution the session
    /// negotiated, so the row applies at the next session. The console's write lands in a child
    /// of this test binary whose config dir is scratch; the parent's is never written.
    #[test]
    fn a_rebuild_opens_the_backend_the_session_negotiated() {
        if std::env::var_os(SCRATCH_CONFIG).is_none() {
            let dir = tempfile::tempdir().expect("scratch config dir");
            let module = module_path!().split_once("::").expect("crate::module").1;
            let test = format!("{module}::a_rebuild_opens_the_backend_the_session_negotiated");
            let mut child =
                std::process::Command::new(std::env::current_exe().expect("this test binary"));
            child.args(["--exact", &test, "--nocapture"]);
            for (key, _) in std::env::vars_os() {
                if key.to_string_lossy().starts_with("PUNKTFUNK_") {
                    child.env_remove(key);
                }
            }
            let out = child
                .env(SCRATCH_CONFIG, dir.path())
                .env("PUNKTFUNK_CONFIG_DIR", dir.path())
                .output()
                .expect("run the child");
            assert!(
                out.status.success(),
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            let wrote = dir.path().join("host-settings.json").exists();
            assert!(wrote, "the child ran no test");
            return;
        }
        let f = fake();
        let session = f.st.facts.encoder.clone();
        let patch = serde_json::json!({ "encoder": "nvenc" });
        pf_host_config::save(patch.as_object().expect("a patch")).expect("the console's write");
        assert!(
            !Arc::ptr_eq(&session, &crate::encode::resolve()),
            "the write moves what the next session resolves"
        );
        let req =
            f.st.build_request(f.st.live.cur_mode, f.st.live.bitrate_kbps);
        let mut on_session = false;
        super::super::pipeline::open_session_encoder_with(
            &req.encoder,
            &req.plan,
            &*f.st.live.capturer,
            &f.st.live.frame,
            (f.st.facts.negotiated.width, f.st.facts.negotiated.height),
            60,
            |_, _| 20_000_000,
            req.bit_depth,
            None,
            req.wire_seq_base,
            |resolved, _| {
                on_session = std::ptr::eq(resolved, &*session);
                Ok(Box::new(NullEnc))
            },
        )
        .expect("an encoder");
        assert!(on_session, "the rebuild opened on the row, not the session");
    }

    /// What one swap site leaves behind. Everything not named here holds for every site: nothing
    /// in flight, the watchdog fed, the wind-back the old encoder owed settled, the IDR clock
    /// armed.
    struct Row {
        site: &'static str,
        why: SwapReason,
        next: fn() -> Next,
        /// The source moved under the old encoder before this swap, as `follow_source_mode` sees it.
        source_moved_first: bool,
        epoch_moved: bool,
        ceiling_cleared: bool,
        retarget_forgotten: bool,
        enc_src_reread: bool,
        retired: Option<u64>,
    }

    const ROWS: &[Row] = &[
        Row {
            site: "session switch",
            why: SwapReason::SessionSwitch,
            next: || pipeline(WIDE),
            source_moved_first: false,
            epoch_moved: true,
            ceiling_cleared: true,
            retarget_forgotten: true,
            enc_src_reread: false,
            retired: None,
        },
        Row {
            site: "mode switch",
            why: SwapReason::ModeSwitch,
            next: || pipeline(SMALL),
            source_moved_first: false,
            epoch_moved: true,
            ceiling_cleared: true,
            retarget_forgotten: true,
            enc_src_reread: true,
            retired: Some(1),
        },
        Row {
            site: "mode switch in place",
            why: SwapReason::ModeSwitch,
            next: || encoder(Some(WIDE)),
            source_moved_first: false,
            epoch_moved: true,
            ceiling_cleared: true,
            retarget_forgotten: true,
            enc_src_reread: true,
            retired: None,
        },
        Row {
            site: "topology re-assert",
            why: SwapReason::TopologyReassert,
            next: || pipeline(SMALL),
            source_moved_first: false,
            epoch_moved: true,
            ceiling_cleared: false,
            retarget_forgotten: true,
            enc_src_reread: true,
            retired: Some(1),
        },
        Row {
            site: "topology re-assert in place",
            why: SwapReason::TopologyReassert,
            next: || encoder(Some(SMALL)),
            source_moved_first: false,
            epoch_moved: true,
            ceiling_cleared: false,
            retarget_forgotten: true,
            enc_src_reread: true,
            retired: None,
        },
        Row {
            site: "topology re-assert in place, onto another frame",
            why: SwapReason::TopologyReassert,
            next: || encoder(Some(WIDE)),
            source_moved_first: false,
            epoch_moved: true,
            ceiling_cleared: true,
            retarget_forgotten: true,
            enc_src_reread: true,
            retired: None,
        },
        Row {
            site: "capture lost",
            why: SwapReason::CaptureLost,
            next: || pipeline(WIDE),
            source_moved_first: false,
            epoch_moved: true,
            ceiling_cleared: true,
            retarget_forgotten: true,
            enc_src_reread: false,
            retired: Some(1),
        },
        Row {
            site: "source mode",
            why: SwapReason::SourceMode,
            next: || encoder(None),
            source_moved_first: true,
            epoch_moved: true,
            ceiling_cleared: true,
            retarget_forgotten: true,
            enc_src_reread: true,
            retired: None,
        },
        Row {
            site: "codec change",
            why: SwapReason::Format,
            next: || pipeline(SMALL),
            source_moved_first: false,
            epoch_moved: true,
            ceiling_cleared: true,
            retarget_forgotten: true,
            enc_src_reread: true,
            retired: Some(1),
        },
        Row {
            site: "bitrate",
            why: SwapReason::Bitrate,
            next: || encoder(None),
            source_moved_first: true,
            epoch_moved: false,
            ceiling_cleared: false,
            retarget_forgotten: false,
            enc_src_reread: false,
            retired: None,
        },
    ];

    #[test]
    fn each_swap_restarts_what_its_reason_says() {
        for row in ROWS {
            let site = row.site;
            let mut session = fake();
            let st = &mut session.st;
            if row.source_moved_first {
                st.live.frame = frame(WIDE);
            }
            let old_gen = st.live.cur_display_gen;

            st.swap_pipeline((row.next)(), row.why);

            assert!(st.clock.inflight.is_empty(), "{site}: nothing in flight");
            assert_eq!(st.clock.watchdog.resets(), 0, "{site}: watchdog fed");
            let p = &st.pipelining;
            assert_eq!(
                (p.deescalating, p.pipelined_active, p.pipeline_asked),
                (false, false, false),
                "{site}: escalation carried"
            );
            assert!(st.clock.last_forced_idr.is_some(), "{site}: IDR clock");
            assert_eq!(st.live.epoch != 7, row.epoch_moved, "{site}: epoch");
            let mut tells = 0;
            while let Ok(d) = session.told.try_recv() {
                assert_eq!(d.epoch, st.live.epoch, "{site}: the epoch it tells");
                tells += 1;
            }
            assert_eq!(
                tells,
                usize::from(row.epoch_moved),
                "{site}: told once per epoch"
            );
            let (_, ceiling) = st.shared.encoder_ceiling.lock().unwrap().resolve(50_000);
            assert_eq!(
                ceiling == AckReason::Granted,
                row.ceiling_cleared,
                "{site}: ceiling"
            );
            assert_eq!(
                (st.live.retargeted, st.fec.pending.is_some()),
                (!row.retarget_forgotten, !row.retarget_forgotten),
                "{site}: retarget state"
            );
            assert_eq!(
                st.live.enc_src == enc_src_of(&st.live.frame),
                row.enc_src_reread,
                "{site}: enc_src"
            );
            assert_eq!(
                row.why.retires(old_gen, st.live.cur_display_gen),
                row.retired,
                "{site}: display lease"
            );
        }
    }

    /// A built pipeline brings its capture with it, and the AU numbering runs on: the new
    /// encoder opened at `au_seq`.
    #[test]
    fn a_built_pipeline_brings_its_capture_and_keeps_the_au_sequence() {
        let mut session = fake();
        let st = &mut session.st;
        st.swap_pipeline(pipeline(WIDE), SwapReason::CaptureLost);
        assert_eq!(st.clock.au_seq, 40);
        assert_eq!((st.live.cur_node_id, st.live.cur_display_gen), (2, Some(2)));
        assert_eq!((st.live.frame.width, st.live.frame.height), WIDE);
    }
}
