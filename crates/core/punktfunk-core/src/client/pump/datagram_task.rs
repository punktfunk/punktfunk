//! Host → client datagram demux. `try_send` drops the newest packet when the embedder
//! lags, so a slow consumer never backs up the QUIC receive path.

use super::super::planes::DatagramTx;
use super::*;

/// Demux host datagrams onto `planes` until the connection closes. `encode_lat` is the ABR's
/// encode and host-timing accumulator, fed here and not from the host-timing plane, which is
/// overlay-lossy and embedder-drained. `shared` holds the audio anchor each desktop audio
/// packet passes.
pub(super) async fn run(
    conn: ClientConn,
    planes: DatagramTx,
    encode_lat: Arc<Mutex<super::super::frame_channel::EncodeLatAcc>>,
    shared: Arc<ClientShared>,
) {
    // Per-pad seq gate for v2 rumble: a reorder must not restart a stopped motor.
    // v1 has no seq and bypasses (the host's periodic re-send is the only heal).
    let mut rumble_last_seq: [Option<u8>; crate::input::MAX_PADS] = [None; crate::input::MAX_PADS];
    // `0xD2` rebuild: recover here and re-insert in order so every embedder sees a
    // complete stream without knowing the plane exists.
    let mut audio_red = crate::audio::AudioRedRecovery::new();
    // One seq space for every audio plane: a late or duplicate packet never reaches a decoder.
    let mut audio_seq = crate::audio::AudioSeqGate::new();
    let mut pad_audio_seq = [[crate::audio::PadSeqGate::default(); 2]; crate::input::MAX_PADS];
    // `0xD4` parts become one access unit. A format mark rides the next whole unit out.
    let mut access_unit = crate::audio::bitstream::AuAssembler::new();
    let mut access_unit_mark = None;
    // Under audio epochs a packet passes, opens a new format, or waits out its config.
    let admit = |seq: u32, codec: u8| {
        shared
            .audio_anchor
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .admit(seq, codec)
    };
    while let Ok(d) = conn.read_datagram().await {
        match d.first() {
            Some(&crate::quic::AUDIO_MAGIC) => {
                if let Some((seq, pts_ns, opus)) =
                    crate::quic::decode_audio_datagram(&d).filter(|&(seq, ..)| audio_seq.fresh(seq))
                {
                    let Some(new_format) = admitted(admit(seq, crate::quic::AUDIO_CODEC_OPUS))
                    else {
                        continue;
                    };
                    // Keeps the RED anchor current, so a later `0xD2` never re-inserts a frame
                    // this plane already delivered.
                    audio_red.recover_before(seq, false);
                    let _ = planes.audio.try_send(AudioPacket {
                        seq,
                        pts_ns,
                        data: opus.to_vec(),
                        new_format,
                    });
                }
            }
            Some(&crate::quic::AUDIO_RED_MAGIC) => {
                if let Some((seq, pts_ns, opus, prev)) = crate::quic::decode_audio_red_datagram(&d)
                    .filter(|&(seq, ..)| audio_seq.fresh(seq))
                {
                    let Some(new_format) = admitted(admit(seq, crate::quic::AUDIO_CODEC_OPUS))
                    else {
                        continue;
                    };
                    // The anchor moves on every packet, or the next one re-inserts this one.
                    // A new format starts here: its predecessor was another plane's.
                    let recover = audio_red.recover_before(seq, prev.is_some());
                    if new_format.is_none() && recover {
                        // Copy is the previous protocol frame: seq-1, pts minus one FRAME_MS.
                        let _ = planes.audio.try_send(AudioPacket {
                            seq: seq.wrapping_sub(1),
                            pts_ns: pts_ns
                                .saturating_sub(crate::audio::FRAME_MS as u64 * 1_000_000),
                            data: prev.unwrap_or_default().to_vec(),
                            new_format: None,
                        });
                    }
                    let _ = planes.audio.try_send(AudioPacket {
                        seq,
                        pts_ns,
                        data: opus.to_vec(),
                        new_format,
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
                        let _ = planes.rumble.try_send((u.pad, u.low, u.high, ttl));
                        planes.rumble_feed.wire_update(
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
                    let _ = planes.hidout.try_send(h);
                }
            }
            Some(&crate::quic::PAD_AUDIO_MAGIC) => {
                // One gate per (pad, kind): a duplicate or reordered frame never reaches the
                // decoder. A jump back past the reorder window is a host streamer restart.
                // A kind this build does not know passes unfiltered.
                if let Some(f) = crate::quic::decode_pad_audio_datagram(&d).filter(|f| {
                    f.kind > crate::quic::PAD_AUDIO_KIND_SPEAKER
                        || pad_audio_seq[usize::from(f.pad & 0x0f)][usize::from(f.kind)]
                            .fresh(f.seq)
                }) {
                    let _ = planes.pad_audio.try_send(f);
                }
            }
            // Same queue as `0xC9` so seq/pts mean the same; `Welcome::audio_codec` is the
            // payload format. A session runs one plane for life. No `0xD2` on PCM —
            // conceal at decode (`pcm::PcmConceal`), do not reconstruct here.
            Some(&crate::quic::AUDIO_PCM_MAGIC) => {
                if let Some((seq, pts_ns, pcm)) = crate::quic::decode_audio_pcm_datagram(&d)
                    .filter(|&(seq, ..)| audio_seq.fresh(seq))
                {
                    let Some(new_format) = admitted(admit(seq, crate::quic::AUDIO_CODEC_PCM))
                    else {
                        continue;
                    };
                    audio_red.recover_before(seq, false);
                    let _ = planes.audio.try_send(AudioPacket {
                        seq,
                        pts_ns,
                        data: pcm.to_vec(),
                        new_format,
                    });
                }
            }
            // Same queue again, one whole access unit per packet. No `0xD2` here either.
            Some(&crate::quic::AUDIO_BITSTREAM_MAGIC) => {
                if let Some((seq, pts_ns, index, count, bytes)) =
                    crate::quic::decode_audio_bitstream_datagram(&d)
                {
                    // The seq gates run once per unit, on part 0; the assembler takes a later
                    // part only as the continuation of a unit that passed them.
                    if index == 0 {
                        if !audio_seq.fresh(seq) {
                            continue;
                        }
                        // The tag names no codec within the family; AC-3 stands for both.
                        let Some(new_format) = admitted(admit(seq, crate::quic::AUDIO_CODEC_AC3))
                        else {
                            continue;
                        };
                        audio_red.recover_before(seq, false);
                        access_unit_mark = new_format.or(access_unit_mark);
                    }
                    if let Some((seq, pts_ns, data)) =
                        access_unit.push(seq, pts_ns, index, count, bytes)
                    {
                        let _ = planes.audio.try_send(AudioPacket {
                            seq,
                            pts_ns,
                            data,
                            new_format: access_unit_mark.take(),
                        });
                    }
                }
            }
            Some(&crate::quic::HDR_META_MAGIC) => {
                if let Some(m) = crate::quic::decode_hdr_meta_datagram(&d) {
                    let _ = planes.hdr_meta.try_send(m);
                }
            }
            Some(&crate::quic::HOST_TIMING_MAGIC) => {
                if let Some(t) = crate::quic::decode_host_timing_datagram(&d) {
                    let mut acc = encode_lat.lock().unwrap();
                    if let Some(s) = &t.stages {
                        acc.sum_us += s.encode_us as u64;
                        acc.count += 1;
                    }
                    if acc.timings.len() < super::super::planes::HOST_TIMING_QUEUE {
                        acc.timings.push(t);
                    }
                    drop(acc);
                    let _ = planes.host_timing.try_send(t);
                }
            }
            Some(&crate::quic::CURSOR_STATE_MAGIC) => {
                if let Some(s) = crate::quic::decode_cursor_state_datagram(&d) {
                    let _ = planes.cursor_state.try_send(s);
                }
            }
            _ => {} // newer host; ignore
        }
    }
}

/// An admitted packet's mark, `None` for a packet that waits out its config.
fn admitted(admit: super::anchor::AudioAdmit) -> Option<Option<crate::audio::plane::PlaneFormat>> {
    match admit {
        super::anchor::AudioAdmit::Pass => Some(None),
        super::anchor::AudioAdmit::New(format) => Some(Some(format)),
        super::anchor::AudioAdmit::Drop => None,
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
        let (planes, rx) = super::super::planes::channels();
        tokio::spawn(run(
            ClientConn::new(client_conn),
            planes.datagram,
            Arc::new(Mutex::new(
                super::super::frame_channel::EncodeLatAcc::default(),
            )),
            Arc::new(ClientShared::new(crate::config::Mode::default())),
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
            let audio = rx.audio.into_inner().unwrap();
            audio.recv_timeout(std::time::Duration::from_secs(5))
        })
        .await
        .unwrap()
        .expect("the 0xD3 arm must feed the audio sink");
        assert_eq!(got.seq, 7);
        assert_eq!(got.pts_ns, 1_234_567);
        assert_eq!(got.data, wire, "the payload must cross unmodified");
    }

    /// The parts of one `0xD4` access unit reach the audio sink as one packet: the bytes
    /// joined, the unit's seq and pts. The next unit takes the next seq.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_split_access_unit_reaches_the_audio_sink_whole() {
        let (_server_ep, _client_ep, host_conn, client_conn) =
            crate::quic::test_util::connect_pair().await;
        let (planes, rx) = super::super::planes::channels();
        tokio::spawn(run(
            ClientConn::new(client_conn),
            planes.datagram,
            Arc::new(Mutex::new(
                super::super::frame_channel::EncodeLatAcc::default(),
            )),
            Arc::new(ClientShared::new(crate::config::Mode::default())),
        ));

        let syncframe: Vec<u8> = (0..1_792u32).map(|i| (i % 251) as u8).collect();
        let (head, tail) = syncframe.split_at(896);
        for (seq, pts, index, count, part) in [
            (40, 9_876_543, 0, 2, head),
            (40, 9_876_543, 1, 2, tail),
            (41, 9_908_543, 0, 1, head),
        ] {
            let d = crate::quic::encode_audio_bitstream_datagram(seq, pts, index, count, part);
            host_conn
                .send_datagram(
                    crate::quic::v2::dgram::wrap(&d)
                        .expect("audio has a kind")
                        .into(),
                )
                .expect("datagram fits the path");
        }

        let got = tokio::task::spawn_blocking(move || {
            let audio = rx.audio.into_inner().unwrap();
            let first = audio.recv_timeout(std::time::Duration::from_secs(5));
            let second = audio.recv_timeout(std::time::Duration::from_secs(5));
            let none = audio.recv_timeout(std::time::Duration::from_millis(200));
            (first, second, none)
        })
        .await
        .unwrap();
        let au = got.0.expect("the 0xD4 arm must feed the audio sink");
        assert_eq!((au.seq, au.pts_ns), (40, 9_876_543));
        assert_eq!(au.data, syncframe, "the parts join in order");
        let next = got.1.expect("the next unit follows");
        assert_eq!(
            (next.seq, next.pts_ns),
            (41, 9_908_543),
            "the seq space stays dense"
        );
        assert_eq!(next.data, head);
        assert!(got.2.is_err(), "one access unit is one packet");
    }
}
