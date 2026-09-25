//! The client worker: QUIC handshake + control/input/datagram tasks + the blocking data-plane pump.

use super::frame_channel::{
    StandingLatAction, StandingLatency, CLOCK_RESYNC_INTERVAL, FLUSH_AFTER, FLUSH_COOLDOWN,
    FLUSH_LATENCY, NOOP_CLOCK_FLUSHES_TO_DISARM, NOOP_FLUSH_DATAGRAMS, PIN_SHEDS_TO_WARN,
    QUEUE_HIGH, QUEUE_LOW, STANDING_TIME,
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
        bitrate_kbps,
        frames,
        audio_tx,
        rumble_tx,
        rumble_feed,
        hidout_tx,
        pad_audio_tx,
        pad_audio_caps,
        pad_mouse,
        scroll_invert,
        hdr_meta_tx,
        host_timing_tx,
        cursor_shape_tx,
        cursor_state_tx,
        input_rx,
        mut mic_rx,
        mut rich_input_rx,
        ctrl_rx,
        ctrl_tx,
        clip_event_tx,
        clip_cmd_rx,
        ready_tx,
        shutdown,
        end_reason,
        quit,
        mode_slot,
        probe,
        frames_dropped,
        fec_recovered,
        unsustainable_pin_kbps,
        mic_stats,
        hot_tids,
        clock_offset,
        rtt_us,
        decode_lat,
        live_bitrate,
        rate_cut,
        abr_windows,
        abr_ramp,
        recent_rfis,
        short_frames,
        audio_mute,
        pad_slots,
        launch_outcome,
        access_grants,
        access_deadline_unix,
        access_tx,
        end_reject_code,
        end_reject_said,
        ..
    } = args;
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
    // refresh, not the request still sitting in `mode_slot` (60-for-120 must score at 60).
    let refresh_hz = negotiated.mode.refresh_hz;
    // Seed before `ready_tx`: `clock_offset_now_ns` must not read a pre-handshake 0.
    clock_offset.store(negotiated.clock_offset_ns, Ordering::Relaxed);
    // Welcome is the starting encoder target (0 if the host reports none).
    live_bitrate.store(negotiated.bitrate_kbps, Ordering::Relaxed);
    // Seed before the embedder observes us, so `access_grants()` never reads
    // GRANT_ALL on a limited session. Deadline is client wall clock: the wire
    // carries relative `expires_in_secs`, so skew does not move the countdown.
    access_grants.store(negotiated.grants, Ordering::Relaxed);
    access_deadline_unix.store(
        access_deadline_from(wall_clock_ns(), negotiated.expires_in_secs),
        Ordering::Relaxed,
    );
    // Bumped when a re-sync batch is applied; the pump resets staleness and re-arms jump-to-live.
    let clock_gen = Arc::new(AtomicU32::new(0));
    // Normalized scroll only toward HOST_CAP2_SCROLL; an older host gets each
    // event converted once at the outbound seam instead.
    let normalized_scroll = negotiated.host_caps2 & crate::quic::HOST_CAP2_SCROLL != 0;
    let _ = ready_tx.send(Ok(negotiated));

    // Snapshots only toward GAMEPAD_STATE. Flags 8/9 only toward PAD_AUDIO — an
    // older host reads the whole flags word as the pad index.
    let gamepad_snapshots = host_caps & crate::quic::HOST_CAP_GAMEPAD_STATE != 0;
    let pad_audio_arrivals = host_caps & crate::quic::HOST_CAP_PAD_AUDIO != 0;
    tokio::spawn(input_task::run(
        conn.clone(),
        input_rx,
        gamepad_snapshots,
        pad_audio_arrivals,
        pad_audio_caps,
        input_task::MouseArgs {
            shared: pad_mouse,
            grants: access_grants.clone(),
            mode: mode_slot.clone(),
            scroll_invert,
            normalized_scroll,
        },
    ));

    // Smoothed path round trip for the overlay, sampled while the connection lives.
    let rtt_conn = conn.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_millis(250));
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    let us = rtt_conn.rtt().as_micros().min(u128::from(u32::MAX)) as u32;
                    rtt_us.store(us, Ordering::Relaxed);
                }
                _ = rtt_conn.closed() => break,
            }
        }
    });

    // 0xCB mic uplink. A frame with more than [`MIC_BACKLOG_MAX`] successors is
    // shed: a stall costs a dropout, not session-long lag ([`MIC_QUEUE`]).
    let mic_conn = conn.clone();
    tokio::spawn(async move {
        while let Some((seq, pts_ns, opus)) = mic_rx.recv().await {
            if mic_rx.len() > MIC_BACKLOG_MAX {
                mic_stats.dropped_stale.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            let d = crate::quic::encode_mic_datagram(seq, pts_ns, &opus);
            let _ = mic_conn.send_datagram(d.into());
            mic_stats.sent.fetch_add(1, Ordering::Relaxed);
        }
    });

    // Pre-encoded 0xCC uplink. Encoded at NativeClient so new plane kinds never touch the pump.
    let rich_conn = conn.clone();
    tokio::spawn(async move {
        while let Some(d) = rich_input_rx.recv().await {
            let _ = rich_conn.send_datagram(d.into());
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
    let mode_slot_pump = mode_slot.clone();
    tokio::spawn(
        control_task::ControlTask {
            ctrl_rx,
            ctrl_send,
            ctrl_recv,
            clock_rtt_ns,
            mode_slot,
            probe: probe.clone(),
            bitrate_ack: bitrate_ack.clone(),
            live_bitrate,
            recovery_kf: recovery_kf.clone(),
            recent_rfis,
            pipeline_gap: pipeline_gap.clone(),
            clock_offset: clock_offset.clone(),
            clock_gen: clock_gen.clone(),
            clip_event_tx: clip_event_tx.clone(),
            cursor_shape_tx,
            mode_gen: mode_gen.clone(),
            audio_mute,
            pad_slots,
            launch_outcome,
            access_grants,
            access_deadline_unix,
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
        conn.clone(),
        clip_event_tx,
        clip_cmd_rx,
    ));

    // Connection close: classify, then shutdown.
    {
        let shutdown = shutdown.clone();
        let end_reason = end_reason.clone();
        let conn = conn.clone();
        tokio::spawn(async move {
            let why = conn.closed().await;
            // Reason before `shutdown`: different threads observe the two; the flag must not win.
            let reason = crate::client::PunktfunkEndReason::from(&why);
            // Mid-session typed close (access expiry, …) beside the coarse reason, same order.
            // The host's sentence lands before the code: a reader that sees the code must
            // not find the text still missing.
            if let Some((r, said)) = reject_from_close(&conn) {
                if !said.is_empty() {
                    let _ = end_reject_said.set(said);
                }
                end_reject_code.store(r.close_code(), Ordering::SeqCst);
            }
            end_reason.store(reason as u8, Ordering::SeqCst);
            shutdown.store(true, Ordering::SeqCst);
        });
    }

    let pump = data::DataPump {
        session,
        frames,
        ctrl_tx,
        shutdown,
        probe,
        hot_tids,
        clock_offset,
        clock_gen,
        decode_lat,
        encode_lat,
        mode_gen,
        frames_dropped,
        fec_recovered,
        unsustainable_pin_kbps,
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
        mode_slot: mode_slot_pump,
        rate_cut,
        abr_windows,
        abr_ramp,
        short_frames,
    };
    let _ = tokio::task::spawn_blocking(move || pump.run()).await;

    // Quit code: host skips keep-alive linger. Close 0: host lingers for reconnect.
    let close_code = if quit.load(Ordering::SeqCst) {
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
