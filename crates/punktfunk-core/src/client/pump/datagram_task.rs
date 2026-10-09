//! Host → client datagram demux. `try_send` drops the newest packet when the embedder
//! lags, so a slow consumer never backs up the QUIC receive path.

use super::*;

// One parameter per demuxed plane; a struct would only move the field list off the call site.
#[allow(clippy::too_many_arguments)]
pub(super) async fn run(
    conn: ClientConn,
    audio_tx: std::sync::mpsc::SyncSender<AudioPacket>,
    rumble_tx: std::sync::mpsc::SyncSender<RumbleUpdate>,
    rumble_feed: super::super::rumble::RumbleFeed,
    hidout_tx: std::sync::mpsc::SyncSender<crate::quic::HidOutput>,
    pad_audio_tx: std::sync::mpsc::SyncSender<crate::quic::PadAudioFrame>,
    hdr_meta_tx: std::sync::mpsc::SyncSender<crate::quic::HdrMeta>,
    host_timing_tx: std::sync::mpsc::SyncSender<crate::quic::HostTiming>,
    // ABR encode accumulator ([`EncodeLatAcc`]). Fed here, not from `host_timing_tx`
    // (that channel is overlay-lossy and embedder-drained).
    encode_lat: Arc<Mutex<super::super::frame_channel::EncodeLatAcc>>,
    cursor_state_tx: std::sync::mpsc::SyncSender<crate::quic::CursorState>,
) {
    // Per-pad seq gate for v2 rumble: a reorder must not restart a stopped motor.
    // v1 has no seq and bypasses (the host's periodic re-send is the only heal).
    let mut rumble_last_seq: [Option<u8>; crate::input::MAX_PADS] = [None; crate::input::MAX_PADS];
    // `0xD2` rebuild: recover here and re-insert in order so every embedder sees a
    // complete stream without knowing the plane exists.
    let mut audio_red = crate::audio::AudioRedRecovery::new();
    // One seq space for every audio plane: a late or duplicate packet never reaches a decoder.
    let mut audio_seq = crate::audio::AudioSeqGate::new();
    while let Ok(d) = conn.read_datagram().await {
        match d.first() {
            Some(&crate::quic::AUDIO_MAGIC) => {
                if let Some((seq, pts_ns, opus)) =
                    crate::quic::decode_audio_datagram(&d).filter(|&(seq, ..)| audio_seq.fresh(seq))
                {
                    let _ = audio_tx.try_send(AudioPacket {
                        seq,
                        pts_ns,
                        data: opus.to_vec(),
                    });
                }
            }
            Some(&crate::quic::AUDIO_RED_MAGIC) => {
                if let Some((seq, pts_ns, opus, prev)) = crate::quic::decode_audio_red_datagram(&d)
                    .filter(|&(seq, ..)| audio_seq.fresh(seq))
                {
                    if audio_red.recover_before(seq, prev.is_some()) {
                        // Copy is the previous protocol frame: seq-1, pts minus one FRAME_MS.
                        let _ = audio_tx.try_send(AudioPacket {
                            seq: seq.wrapping_sub(1),
                            pts_ns: pts_ns
                                .saturating_sub(crate::audio::FRAME_MS as u64 * 1_000_000),
                            data: prev.unwrap_or_default().to_vec(),
                        });
                    }
                    let _ = audio_tx.try_send(AudioPacket {
                        seq,
                        pts_ns,
                        data: opus.to_vec(),
                    });
                }
            }
            Some(&crate::quic::RUMBLE_MAGIC) => {
                if let Some(u) = crate::quic::decode_rumble_envelope(&d) {
                    // Out-of-range pad: drop before either consumer. The seq gate has no
                    // slot, and an embedder would subscript its own per-pad array.
                    let idx = u.pad as usize;
                    if idx >= crate::input::MAX_PADS {
                        continue;
                    }
                    let fresh = match u.envelope {
                        Some(env) => {
                            if crate::input::GamepadSnapshot::seq_newer(
                                env.seq,
                                rumble_last_seq[idx],
                            ) {
                                rumble_last_seq[idx] = Some(env.seq);
                                true
                            } else {
                                false // reorder/duplicate
                            }
                        }
                        None => true,
                    };
                    if fresh {
                        let ttl = u.envelope.map(|e| e.ttl_ms);
                        // Both consumers: legacy queue is the frozen two-handle C ABI
                        // (`next_rumble`/`next_rumble2`); only the policy engine gets triggers.
                        let _ = rumble_tx.try_send((u.pad, u.low, u.high, ttl));
                        rumble_feed.wire_update(
                            u.pad,
                            u.low,
                            u.high,
                            u.left_trigger,
                            u.right_trigger,
                            ttl,
                        );
                    }
                }
            }
            Some(&crate::quic::HIDOUT_MAGIC) => {
                if let Some(h) = HidOutput::decode(&d) {
                    let _ = hidout_tx.try_send(h);
                }
            }
            Some(&crate::quic::PAD_AUDIO_MAGIC) => {
                if let Some(f) = crate::quic::decode_pad_audio_datagram(&d) {
                    let _ = pad_audio_tx.try_send(f);
                }
            }
            // Same queue as `0xC9` so seq/pts mean the same; `Welcome::audio_codec` is the
            // payload format. A session runs one plane for life. No `0xD2` on PCM —
            // conceal at decode (`pcm::PcmConceal`), do not reconstruct here.
            Some(&crate::quic::AUDIO_PCM_MAGIC) => {
                if let Some((seq, pts_ns, pcm)) = crate::quic::decode_audio_pcm_datagram(&d)
                    .filter(|&(seq, ..)| audio_seq.fresh(seq))
                {
                    let _ = audio_tx.try_send(AudioPacket {
                        seq,
                        pts_ns,
                        data: pcm.to_vec(),
                    });
                }
            }
            Some(&crate::quic::HDR_META_MAGIC) => {
                if let Some(m) = crate::quic::decode_hdr_meta_datagram(&d) {
                    let _ = hdr_meta_tx.try_send(m);
                }
            }
            Some(&crate::quic::HOST_TIMING_MAGIC) => {
                if let Some(t) = crate::quic::decode_host_timing_datagram(&d) {
                    if let Some(s) = &t.stages {
                        let mut acc = encode_lat.lock().unwrap();
                        acc.sum_us += s.encode_us as u64;
                        acc.count += 1;
                    }
                    let _ = host_timing_tx.try_send(t);
                }
            }
            Some(&crate::quic::CURSOR_STATE_MAGIC) => {
                if let Some(s) = crate::quic::decode_cursor_state_datagram(&d) {
                    let _ = cursor_state_tx.try_send(s);
                }
            }
            _ => {} // newer host; ignore
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `0xD3` datagram lands in the same queue `0xC9` feeds, seq/pts intact, payload
    /// unmodified. Driven through the real demux loop: the arm is a tag, a sink, and
    /// no `AudioRedRecovery`, and only the loop can be wrong about those.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_lossless_datagram_reaches_the_audio_sink() {
        let (_server_ep, _client_ep, host_conn, client_conn) =
            crate::quic::test_util::connect_pair().await;

        // Keep every receiver alive: a closed sink would fail `try_send` for the wrong reason.
        let (audio_tx, audio_rx) = std::sync::mpsc::sync_channel::<AudioPacket>(8);
        let (rumble_tx, _rumble_rx) = std::sync::mpsc::sync_channel::<RumbleUpdate>(8);
        let (hidout_tx, _hidout_rx) = std::sync::mpsc::sync_channel(8);
        let (pad_audio_tx, _pad_audio_rx) = std::sync::mpsc::sync_channel(8);
        let (hdr_meta_tx, _hdr_meta_rx) = std::sync::mpsc::sync_channel(8);
        let (host_timing_tx, _host_timing_rx) = std::sync::mpsc::sync_channel(8);
        let (cursor_state_tx, _cursor_state_rx) = std::sync::mpsc::sync_channel(8);
        let rumble_feed =
            super::super::rumble::RumbleFeed(Arc::new(super::super::rumble::RumbleShared::new()));
        tokio::spawn(run(
            ClientConn::new(client_conn),
            audio_tx,
            rumble_tx,
            rumble_feed,
            hidout_tx,
            pad_audio_tx,
            hdr_meta_tx,
            host_timing_tx,
            Arc::new(Mutex::new(
                super::super::frame_channel::EncodeLatAcc::default(),
            )),
            cursor_state_tx,
        ));

        // Size from this connection's `max_datagram_size`: the plane is never fragmented,
        // and a 5 ms / 1440 B frame does not fit before MTU discovery settles.
        let bits = crate::audio::pcm::BITS_24;
        let max_dg = host_conn
            .max_datagram_size()
            .expect("datagrams are enabled");
        let frame_us =
            crate::audio::pcm::frame_us_for(crate::audio::SAMPLE_RATE_HZ, bits, 2, max_dg)
                .expect("some rung of the ladder fits");
        let n = crate::audio::pcm::samples_per_frame(crate::audio::SAMPLE_RATE_HZ, frame_us, 2);
        let samples: Vec<f32> = (0..n).map(|i| (i as f32 * 0.01).sin() * 0.8).collect();
        let mut wire = Vec::new();
        crate::audio::pcm::from_f32(&samples, bits, &mut wire);
        assert_eq!(wire.len(), n * 3);
        host_conn
            .send_datagram(
                crate::quic::v2::dgram::wrap(&crate::quic::encode_audio_pcm_datagram(
                    7, 1_234_567, &wire,
                ))
                .expect("audio has a kind")
                .into(),
            )
            .expect("datagram fits the path");

        let got = tokio::task::spawn_blocking(move || {
            audio_rx.recv_timeout(std::time::Duration::from_secs(5))
        })
        .await
        .unwrap()
        .expect("the 0xD3 arm must feed the audio sink");
        assert_eq!(got.seq, 7);
        assert_eq!(got.pts_ns, 1_234_567);
        assert_eq!(got.data, wire, "the payload must cross unmodified");
    }
}
