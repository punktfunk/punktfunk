//! The client worker: QUIC handshake + control/input/datagram tasks + the blocking data-plane pump.

use super::frame_channel::{
    JumpToLive, Shed, StandingLatAction, StandingLatency, CLOCK_RESYNC_INTERVAL, PIN_SHEDS_TO_WARN,
};
use super::worker::reject_from_close;
use super::*;
use crate::config::Role;
use crate::packet::FLAG_PROBE;
use crate::quic::{
    io, wall_clock_ns, BitrateChanged, ClipState, ClockEcho, ClockResync, DeliveryReport, Hello,
    LinkReport, LossReport, ProbeResult, Reconfigure, Reconfigured, RequestKeyframe, ResyncAdmit,
    ResyncGuard, ResyncStep, SetBitrate, Start, Welcome,
};
use crate::session::Session;
use crate::transport::UdpTransport;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

mod control_task;
mod data;
mod datagram_task;
mod handshake;
mod input_task;
mod rx_gap;

/// Host bitrate acks the control task parked for the pump, each with the
/// reason it carried (`None` from a host that does not name its limits).
type AckQueue = std::collections::VecDeque<(u32, Option<crate::quic::AckReason>)>;

/// The control stream's write half, whichever wire carries it.
pub(super) type CtlSend = Box<dyn tokio::io::AsyncWrite + Send + Unpin>;
/// The control stream's read half, whichever wire carries it.
pub(super) type CtlRecv = io::MsgReader<Box<dyn tokio::io::AsyncRead + Send + Unpin>>;

/// The client's connection. On `punktfunk/2` a datagram carries its kind; this adds and strips
/// it, so every task keeps sending and reading the datagrams it always did. Everything else is
/// the quinn connection underneath.
#[derive(Clone)]
pub(super) struct ClientConn {
    conn: quinn::Connection,
    v2: bool,
}

impl ClientConn {
    pub(super) fn new(conn: quinn::Connection, v2: bool) -> ClientConn {
        ClientConn { conn, v2 }
    }

    pub(super) fn is_v2(&self) -> bool {
        self.v2
    }

    pub(super) fn send_datagram(
        &self,
        data: Vec<u8>,
    ) -> std::result::Result<(), quinn::SendDatagramError> {
        if !self.v2 {
            return self.conn.send_datagram(data.into());
        }
        // Every datagram the client sends has a kind; one without is dropped here.
        match crate::quic::v2::dgram::wrap(&data) {
            Some(w) => self.conn.send_datagram(w.into()),
            None => Ok(()),
        }
    }

    /// The next datagram the session logic reads; a kind it takes nothing from is skipped.
    pub(super) async fn read_datagram(
        &self,
    ) -> std::result::Result<Vec<u8>, quinn::ConnectionError> {
        loop {
            let b = self.conn.read_datagram().await?;
            if !self.v2 {
                return Ok(b.to_vec());
            }
            use crate::quic::v2::dgram::{decode, Dgram};
            if let Some(Dgram::Audio(p) | Dgram::InputState(p) | Dgram::HostEvent(p)) = decode(&b) {
                return Ok(p.to_vec());
            }
        }
    }
}

impl std::ops::Deref for ClientConn {
    type Target = quinn::Connection;
    fn deref(&self) -> &quinn::Connection {
        &self.conn
    }
}

/// `punktfunk/1`: datagrams as they are.
impl From<quinn::Connection> for ClientConn {
    fn from(conn: quinn::Connection) -> ClientConn {
        ClientConn::new(conn, false)
    }
}

pub(super) async fn run_pump(args: WorkerArgs) {
    let hs = match handshake::connect_and_handshake(&args).await {
        Ok(hs) => hs,
        Err(e) => {
            let _ = args.ready_tx.send(Err(e));
            return;
        }
    };
    let handshake::HandshakeOut {
        conn,
        ep,
        session,
        ctrl_send,
        ctrl_recv,
        negotiated,
        host_caps,
    } = hs;
    let WorkerArgs {
        params,
        shared,
        audio_tx,
        rumble_tx,
        rumble_feed,
        hidout_tx,
        pad_audio_tx,
        hdr_meta_tx,
        host_timing_tx,
        cursor_shape_tx,
        cursor_state_tx,
        input_rx,
        mut mic_rx,
        mut rich_input_rx,
        pad_touch_rx,
        ctrl_rx,
        ctrl_tx,
        clip_event_tx,
        clip_cmd_rx,
        ready_tx,
        access_tx,
    } = args;
    let bitrate_kbps = params.bitrate_kbps;
    let clock_rtt_ns = negotiated.clock_rtt_ns;
    let resolved_bitrate_kbps = negotiated.bitrate_kbps;
    let negotiated_codec = negotiated.codec;
    // Host marks idle-keepalive repeats (`USER_FLAG_REPEAT`). Only then is an
    // unflagged AU new content; older hosts keep the legacy window arithmetic.
    let marks_repeats = negotiated.host_caps2 & crate::quic::HOST_CAP2_REPEAT_MARK != 0;
    // Host serves probe requests during its own bring-up: measure the link
    // before the first frame instead of bursting beside it.
    let serves_ramp = negotiated.host_caps2 & crate::quic::HOST_CAP2_RAMP != 0;
    // Host divides a path its sessions share, and a delivery count every window
    // is the only thing it can divide by.
    let reads_delivery = negotiated.host_caps2 & crate::quic::HOST_CAP2_DELIVERY != 0;
    // Wire budgets: `actual` is wire bytes plus this audio reservation, spent
    // whether video flows or not. PCM is exact; Opus uses the default-tier ladder
    // (a pinned tier skews a few hundred kbps, inside the ¾ utilization gate).
    let audio_reserved_kbps = if negotiated.audio_codec == crate::quic::AUDIO_CODEC_PCM {
        crate::audio::pcm::bitrate_kbps(
            negotiated.audio_rate_hz,
            negotiated.audio_bits,
            negotiated.audio_channels,
        )
    } else {
        crate::audio::plan_audio_budget(
            negotiated.bitrate_kbps,
            negotiated.audio_channels,
            crate::audio::AudioLayout::from_wire(negotiated.audio_layout).unwrap_or_default(),
            crate::audio::AudioTier::default(),
            host_caps & crate::quic::HOST_CAP_AUDIO_RED != 0,
        )
        .kbps
    };
    // Unchanged across a mode switch; the pump recomputes the stream-shape cap from them.
    let bit_depth = negotiated.bit_depth;
    let chroma_format = negotiated.chroma_format;
    // ABR holds the probe-measured link ceiling to this. Computed here (Welcome
    // geometry); the data pump stays codec-agnostic.
    let stream_cap_kbps = crate::abr::stream_ceiling_kbps(
        negotiated.mode.width,
        negotiated.mode.height,
        negotiated.mode.refresh_hz,
        negotiated.codec,
        negotiated.bit_depth,
        negotiated.chroma_format,
    );
    // ABR encode-threshold unit ([`BitrateController::encode_thresholds`]). Negotiated
    // refresh, not the request still sitting in `shared.mode` (60-for-120 must score at 60).
    let refresh_hz = negotiated.mode.refresh_hz;
    // Seed before `ready_tx`: `clock_offset_now_ns` must not read a pre-handshake 0.
    shared
        .clock_offset
        .store(negotiated.clock_offset_ns, Ordering::Relaxed);
    // Welcome is the starting encoder target (0 if the host reports none).
    shared
        .live_bitrate_kbps
        .store(negotiated.bitrate_kbps, Ordering::Relaxed);
    // Seed before the embedder observes us, so `access_grants()` never reads
    // GRANT_ALL on a limited session. Deadline is client wall clock: the wire
    // carries relative `expires_in_secs`, so skew does not move the countdown.
    shared
        .access_grants
        .store(negotiated.grants, Ordering::Relaxed);
    shared.access_deadline_unix.store(
        access_deadline_from(wall_clock_ns(), negotiated.expires_in_secs),
        Ordering::Relaxed,
    );
    // Bumped when a re-sync batch is applied; the pump resets staleness and re-arms jump-to-live.
    let clock_gen = Arc::new(AtomicU32::new(0));
    // Normalized scroll only toward HOST_CAP2_SCROLL; an older host gets each
    // event converted once at the outbound seam instead.
    let normalized_scroll = negotiated.host_caps2 & crate::quic::HOST_CAP2_SCROLL != 0;
    shared
        .wire
        .store(if conn.is_v2() { 2 } else { 1 }, Ordering::Relaxed);
    let _ = ready_tx.send(Ok(negotiated));

    // Snapshots only toward GAMEPAD_STATE. Flags 8/9 only toward PAD_AUDIO — an
    // older host reads the whole flags word as the pad index.
    let gamepad_snapshots = host_caps & crate::quic::HOST_CAP_GAMEPAD_STATE != 0;
    let pad_audio_arrivals = host_caps & crate::quic::HOST_CAP_PAD_AUDIO != 0;
    // Key edges ride the control stream toward a host that reads them there, so a lost
    // release cannot hold a key; an older host gets every event as a datagram.
    let reliable_edges = negotiated.host_caps2 & crate::quic::HOST_CAP2_INPUT_EDGES != 0;
    tokio::spawn(input_task::run(
        conn.clone(),
        input_rx,
        pad_touch_rx,
        gamepad_snapshots,
        pad_audio_arrivals,
        input_task::MouseArgs {
            client: shared.clone(),
            normalized_scroll,
            edges: reliable_edges.then(|| ctrl_tx.clone()),
        },
    ));

    // Smoothed path round trip for the overlay, sampled while the connection lives.
    let rtt_conn = conn.clone();
    let rtt_shared = shared.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_millis(250));
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    let us = rtt_conn.rtt().as_micros().min(u128::from(u32::MAX)) as u32;
                    rtt_shared.rtt_us.store(us, Ordering::Relaxed);
                }
                _ = rtt_conn.closed() => break,
            }
        }
    });

    // 0xCB mic uplink. A frame with more than [`MIC_BACKLOG_MAX`] successors is
    // shed: a stall costs a dropout, not session-long lag ([`MIC_QUEUE`]).
    let mic_conn = conn.clone();
    let mic_shared = shared.clone();
    tokio::spawn(async move {
        let stats = &mic_shared.mic_stats;
        while let Some((seq, pts_ns, opus)) = mic_rx.recv().await {
            if mic_rx.len() > MIC_BACKLOG_MAX {
                stats.dropped_stale.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            let d = crate::quic::encode_mic_datagram(seq, pts_ns, &opus);
            let _ = mic_conn.send_datagram(d);
            stats.sent.fetch_add(1, Ordering::Relaxed);
        }
    });

    // Pre-encoded 0xCC uplink. Encoded at NativeClient so new plane kinds never touch the pump.
    let rich_conn = conn.clone();
    tokio::spawn(async move {
        while let Some(d) = rich_input_rx.recv().await {
            let _ = rich_conn.send_datagram(d);
        }
    });

    // BitrateChanged queue, drained in order. Not latest-wins: host-cap learning
    // needs two consecutive short acks in the same 750 ms window.
    let bitrate_ack: Arc<Mutex<AckQueue>> = Arc::new(Mutex::new(AckQueue::new()));
    // Outbound `CtrlRequest::Keyframe` count (the one choke point). Pump drains per report window.
    let recovery_kf = Arc::new(AtomicU32::new(0));
    // Host `PipelineGap` length. A local rebuild starves a window without the
    // link failing; drain and discard the in-flight report or ABR sees congestion.
    let pipeline_gap = Arc::new(AtomicU32::new(0));
    // ABR encode signal ([`EncodeLatAcc`]): datagram task samples 0xCF; pump drains a window mean.
    let encode_lat = Arc::new(Mutex::new(super::frame_channel::EncodeLatAcc::default()));
    // Bumped on an accepted mode switch (`clock_gen` pattern). Pump resets host cap and encode baseline.
    let mode_gen = Arc::new(AtomicU32::new(0));

    // Handshake stream stays open ([`control_task`]). When `mode_gen` moves, the
    // data pump re-sizes ABR encode thresholds for the new refresh.
    tokio::spawn(
        control_task::ControlTask {
            ctrl_rx,
            ctrl_send,
            ctrl_recv,
            clock_rtt_ns,
            shared: shared.clone(),
            bitrate_ack: bitrate_ack.clone(),
            recovery_kf: recovery_kf.clone(),
            pipeline_gap: pipeline_gap.clone(),
            clock_gen: clock_gen.clone(),
            clip_event_tx: clip_event_tx.clone(),
            cursor_shape_tx,
            mode_gen: mode_gen.clone(),
            access_tx,
        }
        .run(),
    );

    tokio::spawn(datagram_task::run(
        conn.clone(),
        audio_tx,
        rumble_tx,
        rumble_feed,
        hidout_tx,
        pad_audio_tx,
        hdr_meta_tx,
        host_timing_tx,
        encode_lat.clone(),
        cursor_state_tx,
    ));

    // Bulk clip bytes only; metadata rides the control task. Always spawned: a
    // host without HOST_CAP_CLIPBOARD never opens a clip stream, and offers miss.
    tokio::spawn(crate::clipboard::run(
        (*conn).clone(),
        clip_event_tx,
        clip_cmd_rx,
    ));

    // Connection close: classify, then shutdown. A lost `punktfunk/2` session is kept for the
    // next dial's resume.
    {
        let shared = shared.clone();
        let conn = conn.clone();
        let (host, port) = (params.host.clone(), params.port);
        tokio::spawn(async move {
            let why = conn.closed().await;
            // Reason before `shutdown`: different threads observe the two; the flag must not win.
            let reason = crate::client::PunktfunkEndReason::from(&why);
            let lost_v2 = *shared.v2_session.lock().unwrap_or_else(|e| e.into_inner());
            if let (crate::client::PunktfunkEndReason::Lost, Some(id)) = (reason, lost_v2) {
                crate::client::resume::note_lost(&host, port, id);
            }
            // Mid-session typed close (access expiry, …) beside the coarse reason, same order.
            // The host's sentence lands before the code: a reader that sees the code must
            // not find the text still missing.
            if let Some((r, said)) = reject_from_close(&conn) {
                if !said.is_empty() {
                    let _ = shared.end_reject_said.set(said);
                }
                shared
                    .end_reject_code
                    .store(r.close_code(), Ordering::SeqCst);
            }
            shared.end_reason.store(reason as u8, Ordering::SeqCst);
            shared.shutdown.store(true, Ordering::SeqCst);
        });
    }

    let pump = data::DataPump {
        session,
        shared: shared.clone(),
        ctrl_tx,
        clock_gen,
        encode_lat,
        mode_gen,
        bitrate_ack,
        recovery_kf,
        pipeline_gap,
        bitrate_kbps,
        resolved_bitrate_kbps,
        negotiated_codec,
        bit_depth,
        chroma_format,
        marks_repeats,
        serves_ramp,
        reads_delivery,
        audio_reserved_kbps,
        stream_cap_kbps,
        refresh_hz,
    };
    let _ = tokio::task::spawn_blocking(move || pump.run()).await;

    // Quit code: host skips keep-alive linger. Close 0: host lingers for reconnect.
    let close_code = if shared.quit.load(Ordering::SeqCst) {
        crate::quic::QUIT_CLOSE_CODE
    } else {
        0
    };
    conn.close(close_code.into(), b"client closed");
    // `close` only queues; this `block_on` drops the runtime on return, so flush
    // or the driver never runs and a quit is silence. Bounded so a gone host
    // does not delay exit.
    let _ = tokio::time::timeout(std::time::Duration::from_millis(300), ep.wait_idle()).await;
}
