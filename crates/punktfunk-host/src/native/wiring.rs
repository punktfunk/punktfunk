//! The channels and shared values one session's control task and stream thread talk over.
//!
//! [`SessionWiring::new`] makes each pair once: the control task takes the [`ControlEnds`], the
//! stream thread the [`StreamEnds`], the wire-MTU watcher the [`ShardEnds`], and the first two a
//! clone each of [`SessionShared`]. A new per-session value is added here, not at every hand-off.

use super::*;

/// The mode epoch `epoch` delivers, from the stream thread. `corrects`: it is not what the client
/// was last told, so a client without `StreamConfig` gets a second `Reconfigured`.
pub(crate) struct Delivered {
    pub(crate) mode: punktfunk_core::Mode,
    pub(crate) epoch: u8,
    pub(crate) corrects: bool,
}

/// Values the control task and the stream thread both read or write. A clone shares them.
#[derive(Clone)]
pub(crate) struct SessionShared {
    /// The encoder-applied rate, not the request. With [`Self::encoder_ceiling`] and
    /// [`Self::cadence_degraded`], the encoder truth a `SetBitrate` resolves against.
    pub(crate) live_bitrate: Arc<AtomicU32>,
    /// A ceiling the encoder taught and re-tests (`0` = none). The data plane learns it; the
    /// control task is the one place it decides an ask.
    pub(crate) encoder_ceiling: Arc<std::sync::Mutex<EncoderCeiling>>,
    /// While set, climbs are refused: more bits are not the fix.
    pub(crate) cadence_degraded: Arc<AtomicBool>,
    /// Behind-cadence score for the climb-refusal log (the flag alone has no evidence).
    pub(crate) cadence_behind_score: Arc<AtomicU32>,
    /// Client-received packet count, `u32::MAX` until a `DeliveryReport` (an old client never
    /// sends one). Tells a clean link from a dead one: `loss_ppm = 0` means both.
    pub(crate) client_packets_received: Arc<AtomicU32>,
    /// FEC in force: what the packetizer runs. Only the stream loop writes it.
    pub(crate) fec_target: Arc<AtomicU8>,
    /// Adaptive-FEC proposals, published to `fec_target` only once the encoder accepts the
    /// matching rate.
    pub(crate) fec_requested: Arc<AtomicU8>,
    /// The client's proven link rate (kbps), `0` until its `LinkReport`. The send loop paces a
    /// pinned stream against it.
    pub(crate) link_kbps: Arc<AtomicU32>,
    /// The delivery profile the client asked for (`DeliveryProfile as u8`), written by the
    /// control task and read per frame by the send loop. `PUNKTFUNK_DELIVERY` overrides it.
    pub(crate) delivery: Arc<AtomicU8>,
    /// PhaseReports from the control task; the encode loop drains them at its own cadence
    /// (`design/phase-locked-capture.md`). Inert until a vsync-aware client.
    pub(crate) phase: Arc<stream::PhaseCtl>,
    /// The bring-up ramp's window: probe requests are served on the punched data plane without
    /// the control task's spacing until the send thread takes the session (`stream::ramp`).
    /// Open from the handshake, because the client asks as soon as it has punched.
    pub(crate) ramp_open: Arc<AtomicBool>,
    /// `true` = the client draws the cursor (exclude + forward), `false` = the host composites.
    /// Stays `true`, and inert, for a session without the cursor cap.
    pub(crate) cursor_client_draws: Arc<AtomicBool>,
}

/// The control task's halves.
pub(crate) struct ControlEnds {
    pub(crate) reconfig_tx: std::sync::mpsc::Sender<punktfunk_core::Mode>,
    pub(crate) keyframe_tx: std::sync::mpsc::Sender<()>,
    /// LTR-RFI: the encode loop prefers `invalidate_ref_frames` over a full IDR when it can.
    pub(crate) rfi_tx: std::sync::mpsc::Sender<(u32, u32)>,
    pub(crate) bitrate_tx: std::sync::mpsc::Sender<u32>,
    pub(crate) probe_tx: std::sync::mpsc::Sender<ProbeShaped>,
    pub(crate) probe_result_rx: tokio::sync::mpsc::UnboundedReceiver<ProbeResult>,
    pub(crate) reconfig_result_rx: tokio::sync::mpsc::UnboundedReceiver<Delivered>,
    pub(crate) retarget_rx: tokio::sync::mpsc::UnboundedReceiver<(u32, AckReason)>,
    pub(crate) gap_rx: tokio::sync::mpsc::UnboundedReceiver<u32>,
    /// Wire-MTU watcher → `ShardPayloadChanged`; this task is the control stream's sole
    /// writer. The client's `ShardPayloadAck`s go back on `shard_ack_tx` and gate a grow.
    pub(crate) shard_change_rx: tokio::sync::mpsc::UnboundedReceiver<u16>,
    pub(crate) shard_ack_tx: tokio::sync::mpsc::UnboundedSender<u16>,
    pub(crate) cursor_shape_rx:
        tokio::sync::watch::Receiver<Option<punktfunk_core::quic::CursorShape>>,
}

/// The stream thread's halves.
pub(crate) struct StreamEnds {
    pub(crate) reconfig: std::sync::mpsc::Receiver<punktfunk_core::Mode>,
    pub(crate) keyframe: std::sync::mpsc::Receiver<()>,
    /// Lost-frame range `(first, last)`.
    pub(crate) rfi: std::sync::mpsc::Receiver<(u32, u32)>,
    pub(crate) bitrate_rx: std::sync::mpsc::Receiver<u32>,
    /// Shard re-keys, validated and ack-gated by the wire-MTU watcher. Applied between AUs.
    pub(crate) shard_rx: std::sync::mpsc::Receiver<usize>,
    pub(crate) probe_rx: std::sync::mpsc::Receiver<ProbeShaped>,
    pub(crate) probe_result_tx: tokio::sync::mpsc::UnboundedSender<ProbeResult>,
    /// The accept ack goes out before the rebuild; what each epoch then delivers follows here.
    pub(crate) reconfig_result_tx: tokio::sync::mpsc::UnboundedSender<Delivered>,
    /// The rate the encoder settled on and what settled it, forwarded as `BitrateChanged` so
    /// the client's climb base tracks the encoder: a rebuild re-resolving Automatic is
    /// `Granted`, a short apply `EncoderLimit`.
    pub(crate) retarget_tx: tokio::sync::mpsc::UnboundedSender<(u32, AckReason)>,
    /// Rebuild gap (ms) → `PipelineGap`, so the client discards that ABR window as congestion.
    pub(crate) gap_tx: tokio::sync::mpsc::UnboundedSender<u32>,
    /// Depth-1 latest-wins: the encode loop overwrites a shape the control task has not
    /// drained, so a stalled peer can't grow the host.
    pub(crate) cursor_shape_tx:
        tokio::sync::watch::Sender<Option<punktfunk_core::quic::CursorShape>>,
}

/// The wire-MTU watcher's halves of shard renegotiation ([`wire_mtu::ShardReneg`]). A session
/// that does not renegotiate drops them, which closes the ends the others hold.
pub(crate) struct ShardEnds {
    pub(crate) change_tx: tokio::sync::mpsc::UnboundedSender<u16>,
    pub(crate) ack_rx: tokio::sync::mpsc::UnboundedReceiver<u16>,
    pub(crate) apply_tx: std::sync::mpsc::Sender<usize>,
}

pub(crate) struct SessionWiring {
    pub(crate) control: ControlEnds,
    pub(crate) stream: StreamEnds,
    pub(crate) shard: ShardEnds,
    pub(crate) shared: SessionShared,
}

impl SessionWiring {
    /// Seeded from what Welcome promised. Synthetic-abr aliases `fec_requested` to
    /// `fec_target`: it re-derives frame bytes from FEC every frame and has no retarget to
    /// coordinate.
    pub(crate) fn new(welcome: &Welcome, source: Punktfunk1Source) -> SessionWiring {
        let (reconfig_tx, reconfig) = std::sync::mpsc::channel();
        let (keyframe_tx, keyframe) = std::sync::mpsc::channel();
        let (rfi_tx, rfi) = std::sync::mpsc::channel();
        let (bitrate_tx, bitrate_rx) = std::sync::mpsc::channel();
        let (probe_tx, probe_rx) = std::sync::mpsc::channel();
        let (probe_result_tx, probe_result_rx) = tokio::sync::mpsc::unbounded_channel();
        let (reconfig_result_tx, reconfig_result_rx) = tokio::sync::mpsc::unbounded_channel();
        let (retarget_tx, retarget_rx) = tokio::sync::mpsc::unbounded_channel();
        let (gap_tx, gap_rx) = tokio::sync::mpsc::unbounded_channel();
        let (cursor_shape_tx, cursor_shape_rx) = tokio::sync::watch::channel(None);
        let (change_tx, shard_change_rx) = tokio::sync::mpsc::unbounded_channel();
        let (shard_ack_tx, ack_rx) = tokio::sync::mpsc::unbounded_channel();
        let (apply_tx, shard_rx) = std::sync::mpsc::channel();
        let fec_target = Arc::new(AtomicU8::new(welcome.fec.fec_percent));
        let fec_requested = match source {
            Punktfunk1Source::SyntheticAbr(_) => fec_target.clone(),
            _ => Arc::new(AtomicU8::new(welcome.fec.fec_percent)),
        };
        SessionWiring {
            control: ControlEnds {
                reconfig_tx,
                keyframe_tx,
                rfi_tx,
                bitrate_tx,
                probe_tx,
                probe_result_rx,
                reconfig_result_rx,
                retarget_rx,
                gap_rx,
                shard_change_rx,
                shard_ack_tx,
                cursor_shape_rx,
            },
            stream: StreamEnds {
                reconfig,
                keyframe,
                rfi,
                bitrate_rx,
                shard_rx,
                probe_rx,
                probe_result_tx,
                reconfig_result_tx,
                retarget_tx,
                gap_tx,
                cursor_shape_tx,
            },
            shard: ShardEnds {
                change_tx,
                ack_rx,
                apply_tx,
            },
            shared: SessionShared {
                live_bitrate: Arc::new(AtomicU32::new(welcome.bitrate_kbps)),
                encoder_ceiling: Arc::new(std::sync::Mutex::new(EncoderCeiling::new())),
                cadence_degraded: Arc::new(AtomicBool::new(false)),
                cadence_behind_score: Arc::new(AtomicU32::new(0)),
                client_packets_received: Arc::new(AtomicU32::new(u32::MAX)),
                fec_target,
                fec_requested,
                link_kbps: Arc::new(AtomicU32::new(0)),
                delivery: Arc::new(AtomicU8::new(0)),
                phase: Arc::new(stream::PhaseCtl::new()),
                ramp_open: Arc::new(AtomicBool::new(
                    welcome.host_caps2 & punktfunk_core::quic::HOST_CAP2_RAMP != 0,
                )),
                cursor_client_draws: Arc::new(AtomicBool::new(true)),
            },
        }
    }
}
