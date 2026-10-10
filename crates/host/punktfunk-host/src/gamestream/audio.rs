//! GameStream audio data plane (UDP 48000). On RTSP PLAY we learn the client's
//! endpoint from its port-learning ping, capture the default-sink monitor at the
//! negotiated channel count, Opus-encode fixed frames (or pass 16-bit PCM when
//! Moonlight V+ asks for it), and send each as an RTP audio packet.
//!
//! Wire (moonlight-common-c `AudioStream.c` / `RtpAudioQueue.c`): 12-byte BE
//! `RTP_PACKET` (`packetType = 97`, `sequenceNumber++`,
//! `timestamp += packetDuration`, `ssrc = 0`) then Opus. When negotiated, the
//! payload is AES-128-CBC with PKCS7 and IV `BE32(rikeyid + seq)`; the RTP header
//! stays clear. CBC has no auth tag — that is the protocol. Authenticated audio is
//! the native `punktfunk/1` AES-GCM plane.
//!
//! Stereo is one Opus stream; 5.1/7.1 is libopus multistream. Every layout then
//! emits Sunshine-style FEC: each aligned block of 4 data packets is followed by
//! 2 Reed–Solomon parity packets (`packetType = 127`). Clients consume in-order
//! data immediately; missing parity only costs loss recovery.

use crate::audio::SAMPLE_RATE;
use {
    super::AUDIO_PORT,
    crate::audio::{self, AudioCapSlot, Ready},
    anyhow::{Context, Result},
    cbc::cipher::{block_padding::Pkcs7, BlockModeEncrypt, KeyIvInit},
    std::net::UdpSocket,
    std::sync::atomic::{AtomicBool, Ordering},
    std::sync::Arc,
    std::time::{Duration, Instant},
};

type Aes128CbcEnc = cbc::Encryptor<aes::Aes128>;

/// moonlight-common-c `RtpAudioQueue.c`: `RTP_PAYLOAD_TYPE_AUDIO` / `RTP_PAYLOAD_TYPE_FEC`.
const AUDIO_PACKET_TYPE: u8 = 97;
const AUDIO_FEC_PACKET_TYPE: u8 = 127;

/// Aligned RS block. The client synthesizes the base as `(seq / 4) * 4`, so
/// parity always covers `[base, base+4)`.
const FEC_DATA_SHARDS: usize = 4;
const FEC_PARITY_SHARDS: usize = 2;
/// NVIDIA OpenFEC matrix both ends patch into nanors — not nanors' Cauchy matrix.
/// `parity[j] = XOR_i gfmul(M[j][i], data[i])` (GF(2⁸) poly 0x11d).
const FEC_MATRIX: [[u8; FEC_DATA_SHARDS]; FEC_PARITY_SHARDS] =
    [[0x77, 0x40, 0x38, 0x0e], [0xc7, 0xa7, 0x0d, 0x6c]];

/// RTSP ANNOUNCE audio: `x-nv-audio.surround.numChannels`,
/// `x-nv-audio.surround.AudioQuality`, `x-nv-aqos.packetDuration`. PLAY adds
/// `host_audio` from `/launch`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioParams {
    /// 2, 6 (5.1), 8 (7.1), or Moonlight V+'s 12 (7.1.4).
    pub channels: u8,
    /// `AudioQuality == 1`: uncoupled high-bitrate. Offered only when DESCRIBE
    /// advertises a second surround-params line.
    pub high_quality: bool,
    /// Opus frame duration in ms. Moonlight sends 5, or 10 on a slow decoder/link.
    pub packet_duration_ms: u8,
    /// Client negotiated `SS_ENC_AUDIO` or its legacy feature flag.
    pub encrypt: bool,
    /// Capture must leave the host's own output playing (`localAudioPlayMode`).
    pub host_audio: bool,
    /// 16-bit PCM instead of Opus: Moonlight V+ passthrough (`x-ml-audio.codec=pcm`).
    pub pcm: bool,
}

impl Default for AudioParams {
    fn default() -> Self {
        AudioParams {
            channels: 2,
            high_quality: false,
            packet_duration_ms: 5,
            encrypt: false,
            host_audio: false,
            pcm: false,
        }
    }
}

pub use punktfunk_core::audio::{
    OpusLayout, LAYOUT_51, LAYOUT_51_HQ, LAYOUT_71, LAYOUT_71_HQ, LAYOUT_STEREO,
};

/// Moonlight V+ 7.1.4: the 8-stream, 4-coupled identity layout its decoder builds when DESCRIBE
/// carries no 12-channel line, at Foundation Sunshine's bitrate. Capture stops at 7.1, so the
/// four height channels are silent.
pub const LAYOUT_714: OpusLayout = OpusLayout {
    channels: 12,
    streams: 8,
    coupled: 4,
    mapping: &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11],
    bitrate: 600_000,
};

/// Channels the capture opens: 7.1 at most.
const MAX_CAPTURE_CHANNELS: u8 = 8;

/// Shared [`punktfunk_core::audio::layout_for`], plus V+'s 12 channels ([`LAYOUT_714`]).
/// Unknown channel counts fall back to stereo — stock clients request only 2/6/8.
pub fn layout_for(params: &AudioParams) -> &'static OpusLayout {
    if params.channels == LAYOUT_714.channels {
        return &LAYOUT_714;
    }
    let layout = if params.high_quality {
        punktfunk_core::audio::AudioLayout::Uncoupled
    } else {
        punktfunk_core::audio::AudioLayout::Legacy
    };
    punktfunk_core::audio::layout_for(params.channels, layout)
}

/// `a=fmtp:97 surround-params=` digit string: channelCount, streams, coupledStreams,
/// then one mapping digit per channel.
///
/// moonlight-common-c `parseOpusConfigurations` moves the last NORMAL-quality digit
/// to index 3 (GFE advertised FL FR C RL RR SL SR LFE; the decoder wants LFE at 3).
/// We pre-rotate `adv[3..ch-1] = enc[4..ch]`, `adv[ch-1] = enc[3]` so the post-swap
/// mapping equals the encoder. HQ strings are used verbatim.
///
/// Do not copy Sunshine's `[3, 6)` rotate — that is a config count, not a channel
/// index, and it scrambles 7.1 LFE/SL/SR.
pub fn surround_params(layout: &OpusLayout, high_quality: bool) -> String {
    let ch = layout.channels as usize;
    let mut mapping = layout.mapping.to_vec();
    if !high_quality && ch > 2 {
        mapping[3..ch - 1].copy_from_slice(&layout.mapping[4..ch]);
        mapping[ch - 1] = layout.mapping[3];
    }
    let mut s = format!("{}{}{}", layout.channels, layout.streams, layout.coupled);
    for m in mapping {
        s.push((b'0' + m) as char);
    }
    s
}

/// GF(2⁸) multiply, reduction poly 0x11d (nanors/oblas on both wire ends).
fn gf_mul(mut a: u8, mut b: u8) -> u8 {
    let mut p = 0u8;
    for _ in 0..8 {
        if b & 1 != 0 {
            p ^= a;
        }
        let hi = a & 0x80;
        a <<= 1;
        if hi != 0 {
            a ^= 0x1d;
        }
        b >>= 1;
    }
    p
}

/// RS(4,2) over one aligned block of encrypted payloads, matching nanors
/// `reed_solomon_encode` with [`FEC_MATRIX`]. Unequal shard lengths return `None`
/// and skip the block — FEC is opportunistic, so that only costs recovery.
fn audio_parity(data: &[Vec<u8>]) -> Option<Vec<Vec<u8>>> {
    debug_assert_eq!(data.len(), FEC_DATA_SHARDS);
    let len = data[0].len();
    if data.iter().any(|d| d.len() != len) {
        return None;
    }
    let mut parity = vec![vec![0u8; len]; FEC_PARITY_SHARDS];
    for (j, row) in FEC_MATRIX.iter().enumerate() {
        for (i, shard) in data.iter().enumerate() {
            let coef = row[i];
            for (p, &d) in parity[j].iter_mut().zip(shard.iter()) {
                *p ^= gf_mul(coef, d);
            }
        }
    }
    Some(parity)
}

fn build_rtp(seq: u16, timestamp: u32, opus: &[u8]) -> Vec<u8> {
    let mut p = Vec::with_capacity(12 + opus.len());
    p.push(0x80); // RTP v2, no padding/extension/CSRC
    p.push(AUDIO_PACKET_TYPE);
    p.extend_from_slice(&seq.to_be_bytes());
    p.extend_from_slice(&timestamp.to_be_bytes());
    p.extend_from_slice(&0u32.to_be_bytes());
    p.extend_from_slice(opus);
    p
}

/// FEC datagram: RTP (`packetType = 127`, `timestamp = 0`) + `AUDIO_FEC_HEADER`
/// + parity. moonlight-common-c `RtpAudioQueue.h`.
fn build_fec_rtp(
    rtp_seq: u16,
    shard_index: u8,
    base_seq: u16,
    base_ts: u32,
    parity: &[u8],
) -> Vec<u8> {
    let mut p = Vec::with_capacity(24 + parity.len());
    p.push(0x80);
    p.push(AUDIO_FEC_PACKET_TYPE);
    p.extend_from_slice(&rtp_seq.to_be_bytes());
    p.extend_from_slice(&0u32.to_be_bytes()); // timestamp: Sunshine leaves 0
    p.extend_from_slice(&0u32.to_be_bytes());
    p.push(shard_index);
    p.push(AUDIO_PACKET_TYPE); // stamped onto recovered packets
    p.extend_from_slice(&base_seq.to_be_bytes());
    p.extend_from_slice(&base_ts.to_be_bytes());
    p.extend_from_slice(&0u32.to_be_bytes());
    p.extend_from_slice(parity);
    p
}

/// Spawn the audio thread. `aes_key` is present only when the client negotiated
/// AES-CBC audio; `rikeyid` seeds its per-packet IV.
#[allow(clippy::too_many_arguments)] // one construction site (RTSP PLAY)
pub fn start(
    running: Arc<AtomicBool>,
    aes_key: Option<[u8; 16]>,
    rikeyid: i32,
    params: AudioParams,
    audio_cap: AudioCapSlot,
    on_lost: super::OnSessionLost,
    owner_ip: Option<std::net::IpAddr>,
    // Other half of the endpoint guard beside `owner_ip`.
    av_ping: [u8; super::AV_PING_LEN],
    // Last act of this thread: `/resume` waits on `AppState::media_exited`.
    media_exited: Arc<std::sync::atomic::AtomicU64>,
) {
    let exited = media_exited.clone();
    if std::thread::Builder::new()
        .name("punktfunk-audio".into())
        .spawn(move || {
            tracing::info!(?params, "audio stream starting");
            if let Err(e) = run(
                &running,
                aes_key.as_ref(),
                rikeyid,
                params,
                &audio_cap,
                &on_lost,
                owner_ip,
                &av_ping,
            ) {
                tracing::error!(error = %format!("{e:#}"), "audio stream failed");
            }
            running.store(false, Ordering::SeqCst);
            tracing::info!("audio stream stopped");
            media_exited.fetch_add(1, Ordering::SeqCst);
        })
        .is_err()
    {
        // Never started, so it will not count its exit: count it here, or every later
        // launch waits out its deadline for this thread.
        exited.fetch_add(1, Ordering::SeqCst);
    }
}

#[allow(clippy::too_many_arguments)] // one call site (`start`), which carries the same allow
fn run(
    running: &AtomicBool,
    aes_key: Option<&[u8; 16]>,
    rikeyid: i32,
    params: AudioParams,
    audio_cap: &AudioCapSlot,
    on_lost: &super::OnSessionLost,
    owner_ip: Option<std::net::IpAddr>,
    av_ping: &[u8; super::AV_PING_LEN],
) -> Result<()> {
    let sock = UdpSocket::bind(("0.0.0.0", AUDIO_PORT)).context("bind audio UDP")?;
    punktfunk_core::transport::grow_socket_buffers(&sock);
    tracing::debug!(port = AUDIO_PORT, "audio: awaiting client ping");
    // Same owner-IP + session-ping guard as video. It prevents an off-path peer
    // from diverting the stream or receiving plaintext audio.
    let client = super::learn_client_endpoint(&sock, "audio", owner_ip, av_ping)?;
    sock.connect(client)
        .context("connect client audio endpoint")?;
    // Keep `_qos_flow` for this function's lifetime — Windows qWAVE dies if the
    // flow handle is dropped mid-stream. Applied after `connect` (5-tuple).
    let _qos_flow = punktfunk_core::transport::set_media_qos(
        &sock,
        punktfunk_core::transport::MediaClass::Audio,
    );
    tracing::debug!(%client, "audio: client endpoint learned");

    let want = layout_for(&params).channels.min(MAX_CAPTURE_CHANNELS) as u32;
    // Before the parked-capturer check: it compares the policy this guard sets. Outlives `lease`.
    let _keep_host_audio = params
        .host_audio
        .then(crate::audio::capture_policy::keep_host_audio_guard);
    // Always 48 kHz: GameStream Opus has no rate field, and libopus tops out here.
    // Hi-res `0xD3` is native-only (`design/hi-res-audio.md`).
    let mut lease = audio::CaptureLease::new(want, SAMPLE_RATE, Default::default());
    lease.open(audio_cap, || !running.load(Ordering::SeqCst));
    let result = audio_body(
        &mut lease, &sock, aes_key, rikeyid, params, running, on_lost,
    );
    // Releases the routing claim and parks a live capturer; a dead one is already gone.
    lease.park(audio_cap);
    result
}

fn audio_payload(opus: &[u8], aes_key: Option<&[u8; 16]>, iv_seq: u32) -> Vec<u8> {
    let Some(key) = aes_key else {
        return opus.to_vec();
    };
    let mut iv = [0u8; 16];
    iv[0..4].copy_from_slice(&iv_seq.to_be_bytes());
    Aes128CbcEnc::new(key.into(), (&iv).into()).encrypt_padded_vec::<Pkcs7>(opus)
}

/// Interleaved `from`-channel samples widened to `to` channels with silent extras.
fn pad_channels(samples: impl Iterator<Item = f32>, from: usize, to: usize) -> Vec<f32> {
    let samples: Vec<f32> = samples.collect();
    if from >= to {
        return samples;
    }
    samples
        .chunks_exact(from)
        .flat_map(|s| s.iter().copied().chain(std::iter::repeat_n(0.0, to - from)))
        .collect()
}

/// Interleaved f32 → clamped little-endian i16, what V+ writes straight into an Android
/// `ENCODING_PCM_16BIT` track. Returns the bytes written.
fn pcm_s16le(frame: &[f32], out: &mut [u8]) -> usize {
    for (s, o) in frame.iter().zip(out.chunks_exact_mut(2)) {
        o.copy_from_slice(&((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes());
    }
    frame.len() * 2
}

/// Longest capture wait per loop turn: how late a stop is seen.
const STOP_POLL: Duration = Duration::from_millis(100);
/// How far behind the pacer may fall before it re-anchors instead of catching up.
const PACE_REANCHOR: Duration = Duration::from_millis(100);

#[allow(clippy::too_many_arguments)]
fn audio_body(
    lease: &mut audio::CaptureLease,
    sock: &UdpSocket,
    aes_key: Option<&[u8; 16]>,
    rikeyid: i32,
    params: AudioParams,
    running: &AtomicBool,
    // Tear the whole session down on send failure; otherwise video keeps streaming
    // at the dead endpoint (`AppState::end_session`).
    on_lost: &super::OnSessionLost,
) -> Result<()> {
    let layout = layout_for(&params);
    // Hard CBR: FEC shards must be equal length, and the client asserts a constant
    // per-stream TOC. PCM needs no encoder and is one size by construction.
    let mut enc = if params.pcm {
        None
    } else {
        Some(
            audio::OpusEnc::new(layout, layout.bitrate, audio::RateControl::HardCbr)
                .context("create Opus encoder")?,
        )
    };
    // Snap to a legal Opus frame (48 kHz × {5,10} ms = 240/480). Parse already
    // clamps; a bad value here would reach the encoder.
    let frame_ms = if params.packet_duration_ms >= 10 {
        10
    } else {
        5
    } as usize;
    let samples_per_channel = SAMPLE_RATE as usize * frame_ms / 1000;
    let frame_len = samples_per_channel * layout.channels as usize;
    let mut acc: Vec<f32> = Vec::with_capacity(frame_len * 4);
    let mut out = vec![0u8; (frame_len * 2).max(1400)];
    let mut seq: u16 = 0;
    let mut timestamp: u32 = 0;
    let mut sent: u64 = 0;
    // FEC covers the payload exactly as sent, plaintext or ciphertext. The client
    // RS(4,2) path is not channel-gated.
    let fec = true;
    let mut fec_block: Vec<Vec<u8>> = Vec::with_capacity(FEC_DATA_SHARDS);
    let (mut fec_base_seq, mut fec_base_ts) = (0u16, 0u32);
    let mut fec_skipped = false;
    // Pace to packet duration. PipeWire hands ~1024-frame chunks; bursting them
    // glitches the client's low-latency jitter buffer.
    let frame_dur = Duration::from_millis(frame_ms as u64);
    let mut due = Instant::now();
    // Soft-limited capture gain (`PUNKTFUNK_AUDIO_GAIN`); do not clamp — see
    // `crate::audio::capture_gain`.
    let gain = crate::audio::capture_gain();
    tracing::info!(
        channels = layout.channels,
        streams = layout.streams,
        coupled = layout.coupled,
        bitrate = layout.bitrate,
        frame_ms,
        fec,
        pcm = params.pcm,
        "audio: encoder configured"
    );

    let stopped = || !running.load(Ordering::SeqCst);
    while !stopped() {
        match lease.ready(stopped) {
            Ready::Live => {}
            Ready::Down => continue,
            // The partial frame and the open FEC block straddle the gap. The next block starts
            // at `seq % 4 == 0`, so parity stays aligned.
            Ready::Reopened => {
                acc.clear();
                fec_block.clear();
            }
        }
        // Bounded so a stop is seen well inside `/resume`'s wait for this thread; a quiet
        // host would otherwise hold port 48000 for the backend's whole 5 s timeout.
        let Some(chunk) = lease.next_chunk(Some(STOP_POLL)) else {
            continue;
        };
        // Fewer capture channels than the layout (7.1.4) are padded with silence per frame.
        let cap_ch = lease.live().channels() as usize;
        let frame_in = samples_per_channel * cap_ch;
        acc.extend_from_slice(&chunk);
        while acc.len() >= frame_in {
            let mut frame = pad_channels(acc.drain(..frame_in), cap_ch, layout.channels.into());
            if gain != 1.0 {
                punktfunk_core::audio::apply_gain(&mut frame, gain);
            }
            let n = match enc.as_mut() {
                Some(enc) => enc.encode_float(&frame, &mut out).context("opus encode")?,
                None => pcm_s16le(&frame, &mut out),
            };
            let iv_seq = (rikeyid as u32).wrapping_add(seq as u32);
            let payload = audio_payload(&out[..n], aes_key, iv_seq);
            let pkt = build_rtp(seq, timestamp, &payload);
            if sock.send(&pkt).is_err() {
                tracing::info!(sent, "audio: client unreachable — ending session");
                on_lost();
                return Ok(());
            }
            // RTP seq continues through parity, like Sunshine; the client places
            // shards by the FEC header, not the RTP seq.
            if fec {
                if seq % FEC_DATA_SHARDS as u16 == 0 {
                    fec_block.clear();
                    fec_base_seq = seq;
                    fec_base_ts = timestamp;
                }
                fec_block.push(payload);
                if fec_block.len() == FEC_DATA_SHARDS {
                    match audio_parity(&fec_block) {
                        Some(parity) => {
                            for (x, par) in parity.iter().enumerate() {
                                let rtp_seq =
                                    fec_base_seq.wrapping_add((FEC_DATA_SHARDS + x) as u16);
                                let fp =
                                    build_fec_rtp(rtp_seq, x as u8, fec_base_seq, fec_base_ts, par);
                                if sock.send(&fp).is_err() {
                                    tracing::info!(
                                        sent,
                                        "audio: client unreachable — ending session"
                                    );
                                    on_lost();
                                    return Ok(());
                                }
                            }
                        }
                        None if !fec_skipped => {
                            tracing::warn!("audio: unequal packet sizes — FEC block skipped");
                            fec_skipped = true;
                        }
                        None => {}
                    }
                    fec_block.clear();
                }
            }
            seq = seq.wrapping_add(1);
            // GameStream audio RTP timestamp ticks by packetDuration (ms), not samples.
            timestamp = timestamp.wrapping_add(frame_ms as u32);
            sent += 1;
            if sent % 400 == 0 {
                tracing::debug!(sent, "audio: streaming");
            }

            // Sleep only when ahead; a capture burst must not queue sleeps. Past
            // `PACE_REANCHOR` behind, a quiet stretch is forgiven: chasing it would send
            // every later chunk as a burst.
            due += frame_dur;
            let now = Instant::now();
            if due > now {
                std::thread::sleep((due - now).min(Duration::from_millis(20)));
            } else if now.duration_since(due) > PACE_REANCHOR {
                due = now;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rtp_header_layout() {
        let p = build_rtp(0x0102, 0x03040506, &[0xaa, 0xbb]);
        assert_eq!(p[0], 0x80);
        assert_eq!(p[1], 97);
        assert_eq!(&p[2..4], &[0x01, 0x02]);
        assert_eq!(&p[4..8], &[0x03, 0x04, 0x05, 0x06]);
        assert_eq!(&p[8..12], &[0, 0, 0, 0]);
        assert_eq!(&p[12..], &[0xaa, 0xbb]);
    }

    /// V+ 7.1.4: a 7.1 capture widened to 12 channels encodes with the layout V+ decodes.
    #[test]
    fn layout_714_pads_heights_and_encodes() {
        let p = AudioParams {
            channels: 12,
            high_quality: true, // no HQ 12-channel line is advertised; the layout stays 8/4
            ..AudioParams::default()
        };
        assert_eq!(layout_for(&p), &LAYOUT_714);
        let frame = pad_channels([1.0f32; 8 * 240].into_iter(), 8, 12);
        assert_eq!(frame.len(), 12 * 240);
        assert_eq!(
            &frame[..12],
            &[1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 0.0, 0.0, 0.0, 0.0]
        );
        let mut enc =
            audio::OpusEnc::new(&LAYOUT_714, LAYOUT_714.bitrate, audio::RateControl::HardCbr)
                .unwrap();
        let mut out = vec![0u8; 1400];
        assert!(enc.encode_float(&frame, &mut out).unwrap() > 0);
    }

    #[test]
    fn pcm_is_clamped_little_endian_i16() {
        let mut out = [0u8; 8];
        assert_eq!(pcm_s16le(&[0.0, 1.0, -1.0, 2.0], &mut out), 8);
        assert_eq!(out, [0x00, 0x00, 0xff, 0x7f, 0x01, 0x80, 0xff, 0x7f]);
    }

    #[test]
    fn frame_sizing() {
        assert_eq!(SAMPLE_RATE as usize * 5 / 1000, 240);
        assert_eq!(SAMPLE_RATE as usize * 10 / 1000, 480);
    }

    #[test]
    fn audio_payload_is_sealed_only_with_a_negotiated_key() {
        let opus = [0x11, 0x22, 0x33];
        assert_eq!(audio_payload(&opus, None, 7), opus);
        let sealed = audio_payload(&opus, Some(&[0x44; 16]), 7);
        assert_ne!(sealed, opus);
        assert_eq!(sealed.len() % 16, 0, "CBC payload is PKCS7 padded");
    }

    #[test]
    fn fec_packet_layout() {
        let p = build_fec_rtp(0x1234, 1, 0x1230, 0xAABBCCDD, &[0xEE, 0xFF]);
        assert_eq!(p[0], 0x80);
        assert_eq!(p[1], 127);
        assert_eq!(&p[2..4], &[0x12, 0x34]);
        assert_eq!(&p[4..8], &[0, 0, 0, 0]);
        assert_eq!(&p[8..12], &[0, 0, 0, 0]);
        assert_eq!(p[12], 1);
        assert_eq!(p[13], 97);
        assert_eq!(&p[14..16], &[0x12, 0x30]);
        assert_eq!(&p[16..20], &[0xAA, 0xBB, 0xCC, 0xDD]);
        assert_eq!(&p[20..24], &[0, 0, 0, 0]);
        assert_eq!(&p[24..], &[0xEE, 0xFF]);
    }

    /// Locked SDP strings. Changing a digit breaks the mapping stock Moonlight
    /// derives for its decoder.
    #[test]
    fn surround_params_strings() {
        assert_eq!(surround_params(&LAYOUT_STEREO, false), "21101");
        assert_eq!(surround_params(&LAYOUT_51, false), "642012453");
        assert_eq!(surround_params(&LAYOUT_51_HQ, true), "660012345");
        assert_eq!(surround_params(&LAYOUT_71, false), "85301245673");
        assert_eq!(surround_params(&LAYOUT_71_HQ, true), "88001234567");
    }

    /// moonlight-common-c `RtspConnection.c` normal-quality swap:
    /// `mapping[3] = old[ch-1]`; `mapping[4..] = old[3..ch-1]`.
    fn client_swap(adv: &[u8]) -> Vec<u8> {
        let ch = adv.len();
        let mut m = adv.to_vec();
        m[3] = adv[ch - 1];
        m[4..ch].copy_from_slice(&adv[3..ch - 1]);
        m
    }

    /// Encoder mapping must equal what the client derives: GFE-swap for normal
    /// quality, verbatim for HQ.
    #[test]
    fn client_derived_mapping_matches_encoder() {
        for (layout, hq) in [
            (&LAYOUT_51, false),
            (&LAYOUT_51_HQ, true),
            (&LAYOUT_71, false),
            (&LAYOUT_71_HQ, true),
        ] {
            let s = surround_params(layout, hq);
            let digits: Vec<u8> = s.bytes().map(|b| b - b'0').collect();
            assert_eq!(digits[0], layout.channels);
            assert_eq!(digits[1], layout.streams);
            assert_eq!(digits[2], layout.coupled);
            let adv = &digits[3..];
            let client = if hq { adv.to_vec() } else { client_swap(adv) };
            assert_eq!(
                client, layout.mapping,
                "layout {}ch hq={hq}",
                layout.channels
            );
        }
    }

    fn gf_inv(a: u8) -> u8 {
        (1..=255u8).find(|&b| gf_mul(a, b) == 1).unwrap()
    }

    /// Erase any 2 data shards and recover them from the remaining 2×2 over GF(2⁸).
    /// Matrix and gemm match moonlight-common-c `RtpAudioQueue.c` / `nanors/rs.c`.
    #[test]
    fn fec_parity_recovers_two_losses() {
        let data: Vec<Vec<u8>> = vec![
            vec![0x11, 0x22, 0x33],
            vec![0x44, 0x55, 0x66],
            vec![0x77, 0x88, 0x99],
            vec![0xaa, 0xbb, 0xcc],
        ];
        let parity = audio_parity(&data).unwrap();
        for (e0, e1) in [(0usize, 1usize), (1, 3), (0, 3), (2, 3)] {
            // parity[j] = sum_i M[j][i] d[i]  →  with d[e0], d[e1] unknown:
            //   M[j][e0]·x + M[j][e1]·y = parity[j] ^ sum_{i∉{e0,e1}} M[j][i]·d[i]
            let mut rhs = [parity[0].clone(), parity[1].clone()];
            for j in 0..2 {
                for (i, d) in data.iter().enumerate() {
                    if i != e0 && i != e1 {
                        for (r, &b) in rhs[j].iter_mut().zip(d.iter()) {
                            *r ^= gf_mul(FEC_MATRIX[j][i], b);
                        }
                    }
                }
            }
            // Cramer over GF(2⁸): det = a·d ^ b·c (addition is XOR).
            let (a, b) = (FEC_MATRIX[0][e0], FEC_MATRIX[0][e1]);
            let (c, d) = (FEC_MATRIX[1][e0], FEC_MATRIX[1][e1]);
            let det = gf_mul(a, d) ^ gf_mul(b, c);
            assert_ne!(det, 0, "erasures {e0},{e1} must be solvable");
            let det_inv = gf_inv(det);
            for k in 0..data[0].len() {
                let (r0, r1) = (rhs[0][k], rhs[1][k]);
                let x = gf_mul(det_inv, gf_mul(d, r0) ^ gf_mul(b, r1));
                let y = gf_mul(det_inv, gf_mul(c, r0) ^ gf_mul(a, r1));
                assert_eq!(x, data[e0][k], "shard {e0} byte {k}");
                assert_eq!(y, data[e1][k], "shard {e1} byte {k}");
            }
        }
    }

    /// Unequal shards return `None` — do not emit mixed-length parity.
    #[test]
    fn fec_parity_rejects_unequal_shards() {
        let data = vec![vec![0u8; 10], vec![0u8; 10], vec![0u8; 9], vec![0u8; 10]];
        assert!(audio_parity(&data).is_none());
    }

    /// Encode with our layout, decode with the client's GFE-swapped mapping: a tone
    /// on each input channel must come out on the same output channel.
    #[test]
    fn multistream_51_roundtrip_channel_identity() {
        let layout = &LAYOUT_51;
        let samples = 240; // 5 ms
        let ch = layout.channels as usize;

        let s = surround_params(layout, false);
        let digits: Vec<u8> = s.bytes().map(|b| b - b'0').collect();
        let client_mapping = client_swap(&digits[3..]);

        let mut dec =
            opus::MSDecoder::new(SAMPLE_RATE, layout.streams, layout.coupled, &client_mapping)
                .expect("multistream decoder");

        for tone_ch in 0..ch {
            let mut enc = opus::MSEncoder::new(
                SAMPLE_RATE,
                layout.streams,
                layout.coupled,
                layout.mapping,
                opus::Application::LowDelay,
            )
            .expect("multistream encoder");
            let mut out = vec![0u8; 1400];
            let mut energy = vec![0f64; ch];
            // 8 frames: skip the first 4 so energy is measured past the codec transient.
            for f in 0..8 {
                let mut frame = vec![0f32; samples * ch];
                for t in 0..samples {
                    let phase = (f * samples + t) as f32 * 440.0 * 2.0 * std::f32::consts::PI
                        / SAMPLE_RATE as f32;
                    frame[t * ch + tone_ch] = 0.5 * phase.sin();
                }
                let n = enc.encode_float(&frame, &mut out).unwrap();
                assert!(n > 0);
                let mut decoded = vec![0f32; samples * ch];
                let got = dec.decode_float(&out[..n], &mut decoded, false).unwrap();
                assert_eq!(got, samples);
                if f >= 4 {
                    for t in 0..samples {
                        for (c, e) in energy.iter_mut().enumerate() {
                            *e += (decoded[t * ch + c] as f64).powi(2);
                        }
                    }
                }
            }
            let loudest = (0..ch)
                .max_by(|&a, &b| energy[a].total_cmp(&energy[b]))
                .unwrap();
            assert_eq!(
                loudest, tone_ch,
                "tone in input channel {tone_ch} must come out on output channel {tone_ch} \
                 (energies: {energy:?})"
            );
        }
    }

    /// Live 5.1 capture → encode → decode. Needs
    /// `pactl load-module module-null-sink sink_name=pf51 channels=6 rate=48000`
    /// as the default sink, then
    /// `cargo test -p punktfunk-host -- --ignored surround_capture`.
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore]
    fn surround_capture_live() {
        let mut cap = crate::audio::open_audio_capture(6, SAMPLE_RATE).expect("open 6ch capture");
        let layout = &LAYOUT_51;
        let mut enc =
            audio::OpusEnc::new(layout, layout.bitrate, audio::RateControl::HardCbr).unwrap();
        let mut out = vec![0u8; 1400];
        let mut acc: Vec<f32> = Vec::new();
        let frame_len = 240 * 6;
        let mut packets = 0;
        let mut sizes = std::collections::BTreeSet::new();
        while packets < 100 {
            let chunk = cap.next_chunk().expect("capture chunk");
            acc.extend_from_slice(&chunk);
            while acc.len() >= frame_len && packets < 100 {
                let frame: Vec<f32> = acc.drain(..frame_len).collect();
                let n = enc.encode_float(&frame, &mut out).unwrap();
                sizes.insert(n);
                packets += 1;
            }
        }
        assert_eq!(sizes.len(), 1, "CBR sizes: {sizes:?}");
        let s = surround_params(layout, false);
        let digits: Vec<u8> = s.bytes().map(|b| b - b'0').collect();
        let client_mapping = client_swap(&digits[3..]);
        let mut dec =
            opus::MSDecoder::new(SAMPLE_RATE, layout.streams, layout.coupled, &client_mapping)
                .unwrap();
        let mut pcm = vec![0f32; 240 * 6];
        let got = dec
            .decode_float(&out[..*sizes.first().unwrap()], &mut pcm, false)
            .unwrap();
        assert_eq!(got, 240);
    }
}
