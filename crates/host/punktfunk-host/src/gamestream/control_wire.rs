//! The codec half of the GameStream control stream: the AES-GCM scheme sweep that opens a
//! client's packets, the host-side seal, and the parsers for the loss-recovery messages.
//! `std` and `aes-gcm` only, with no host state, so a fuzz target includes this file by path;
//! every byte here arrives from a LAN peer under the session key alone. What a message does
//! to the session is [`super::control`]'s.

/// Lost-frame range from invalidate-reference-frames (0x0301): two LE `i64`
/// (firstFrame, lastFrame) after `[u16 type][u16 length]`, matching
/// `IDX_INVALIDATE_REF_FRAMES`. `None` if short or nonsensical → caller does a full IDR.
pub(super) fn decode_rfi_range(pt: &[u8]) -> Option<(i64, i64)> {
    if pt.len() < 20 {
        return None;
    }
    let first = i64::from_le_bytes(pt[4..12].try_into().ok()?);
    let last = i64::from_le_bytes(pt[12..20].try_into().ok()?);
    (first >= 0 && last >= first).then_some((first, last))
}

/// Data shards parity could not restore, from a per-frame FEC status (0x5502,
/// `SS_FRAME_FEC_STATUS`: 21 packed BE bytes after `[u16 type][u16 length]`). Loss FEC
/// recovered counts 0: it is FEC doing its job, not a reason to cut the bitrate.
pub(super) fn decode_fec_status_loss(pt: &[u8]) -> Option<u64> {
    let body = pt.get(4..25)?;
    let be16 = |at: usize| u64::from(u16::from_be_bytes([body[at], body[at + 1]]));
    let (total_data, received_data, received_parity) = (be16(10), be16(14), be16(16));
    Some(total_data.saturating_sub(received_data + received_parity))
}

/// What one decrypted control plaintext asks of the session, by its `[u16 type LE]`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ControlMsg {
    /// 0x0301 with a usable range: invalidate these reference frames (NVENC RFI).
    InvalidateFrames(i64, i64),
    /// 0x0302 / 0x0305, or a 0x0301 whose range does not parse: a keyframe. Carries the type.
    Keyframe(u16),
    /// 0x0201 Gen7 loss stats: packets lost since the last report (never negative), the
    /// report window and the last good frame. A client that sees our Sunshine version
    /// sends 0x5502 instead.
    LossStats {
        lost: u64,
        window_ms: i32,
        last_good: i32,
    },
    /// 0x5502 per-frame FEC status, sent when FEC had work to do: data shards parity could
    /// not restore. Zero is FEC doing its job.
    FecLoss(u64),
    /// A 0x5502 whose body does not parse: dropped.
    Ignored,
    /// Everything else — input, keepalives, QoS, a short 0x0201: the input decoders decide.
    Input,
}

impl ControlMsg {
    /// Pin: `IDX_INVALIDATE_REF_FRAMES`, `IDX_LOSS_STATS` (LE i32s `[0]` lost, `[1]` window
    /// ms, `[3]` last-good frame), `SS_FRAME_FEC_STATUS`.
    pub(super) fn parse(pt: &[u8]) -> ControlMsg {
        let Some(ty) = pt.get(0..2) else {
            return ControlMsg::Input;
        };
        let ty = u16::from_le_bytes([ty[0], ty[1]]);
        match ty {
            0x0301 => match decode_rfi_range(pt) {
                Some((first, last)) => ControlMsg::InvalidateFrames(first, last),
                None => ControlMsg::Keyframe(ty),
            },
            0x0302 | 0x0305 => ControlMsg::Keyframe(ty),
            0x0201 if pt.len() >= 20 => ControlMsg::LossStats {
                lost: i32::from_le_bytes(pt[4..8].try_into().expect("len checked")).max(0) as u64,
                window_ms: i32::from_le_bytes(pt[8..12].try_into().expect("len checked")),
                last_good: i32::from_le_bytes(pt[16..20].try_into().expect("len checked")),
            },
            0x5502 => decode_fec_status_loss(pt).map_or(ControlMsg::Ignored, ControlMsg::FecLoss),
            _ => ControlMsg::Input,
        }
    }
}

/// How a control packet's nonce is built. Moonlight picks one from the negotiated flags.
#[derive(Clone, Copy, Debug)]
pub(super) enum NonceKind {
    /// `SS_ENC_CONTROL_V2`: 12-byte nonce, `seq` in [0..4], marker bytes at [10..12].
    V2 { seq_be: bool, marker: [u8; 2] },
    /// Legacy: 16-byte nonce, only `iv[0] = seq & 0xff` (the rest zero).
    LegacyLowByte,
    /// Legacy variant: 16-byte nonce, full `seq` in [0..4] (the rest zero).
    Legacy16Seq { seq_be: bool },
}

impl NonceKind {
    pub(super) fn nonce(&self, seq: u32) -> Vec<u8> {
        let seq_bytes = |be: bool| {
            if be {
                seq.to_be_bytes()
            } else {
                seq.to_le_bytes()
            }
        };
        match *self {
            NonceKind::V2 { seq_be, marker } => {
                let mut iv = vec![0u8; 12];
                iv[0..4].copy_from_slice(&seq_bytes(seq_be));
                iv[10] = marker[0];
                iv[11] = marker[1];
                iv
            }
            NonceKind::LegacyLowByte => {
                let mut iv = vec![0u8; 16];
                iv[0] = (seq & 0xff) as u8;
                iv
            }
            NonceKind::Legacy16Seq { seq_be } => {
                let mut iv = vec![0u8; 16];
                iv[0..4].copy_from_slice(&seq_bytes(seq_be));
                iv
            }
        }
    }
}

/// GCM scheme that opened a control packet. Locked once per connection: AES-GCM gives
/// no partial credit, so an authenticating combination is proof.
#[derive(Clone, Copy, Debug)]
pub(super) struct Scheme {
    /// `gcm_key` is byte-reversed before use (defensive; Sunshine's net effect is forward).
    pub(super) key_rev: bool,
    pub(super) nonce: NonceKind,
    /// GCM tag sits before the ciphertext (vs after).
    pub(super) tag_first: bool,
    pub(super) aad: Aad,
}

#[derive(Clone, Copy, Debug)]
pub(super) enum Aad {
    None,
    /// The 4-byte cleartext header prefix (encType + length), `d[0..4]`.
    Header4,
}

impl Scheme {
    pub(super) fn key(&self, base: &[u8; 16]) -> [u8; 16] {
        let mut k = *base;
        if self.key_rev {
            k.reverse();
        }
        k
    }
}

/// Open encrypted control packet `d` (8-byte cleartext header + `[tag?][ciphertext]`).
/// Fast path: only `detected`. Otherwise sweep nonce × key order × tag position × AAD
/// and return the combination whose GCM tag authenticates.
pub(super) fn decrypt_control(
    key: &[u8; 16],
    d: &[u8],
    detected: &Option<Scheme>,
) -> Option<(Scheme, Vec<u8>)> {
    let seq = u32::from_le_bytes([d[4], d[5], d[6], d[7]]);
    let payload = &d[8..];
    if payload.len() < 16 {
        return None;
    }

    let attempt = |s: Scheme| -> Option<Vec<u8>> {
        // aes-gcm wants `ciphertext || tag`; reassemble from whichever wire order this is.
        let (ct, tag) = if s.tag_first {
            (&payload[16..], &payload[..16])
        } else {
            (
                &payload[..payload.len() - 16],
                &payload[payload.len() - 16..],
            )
        };
        let mut ct_tag = Vec::with_capacity(ct.len() + 16);
        ct_tag.extend_from_slice(ct);
        ct_tag.extend_from_slice(tag);
        let aad: &[u8] = match s.aad {
            Aad::None => &[],
            Aad::Header4 => &d[0..4],
        };
        gcm_open(&s.key(key), &s.nonce.nonce(seq), &ct_tag, aad)
    };

    if let Some(s) = *detected {
        return attempt(s).map(|pt| (s, pt));
    }

    // Candidate nonce constructions, tried in order; the first to authenticate is locked. Below
    // seq 256 `LegacyLowByte` and little-endian `Legacy16Seq` are one nonce, so the former locks.
    const MARKERS: [[u8; 2]; 3] = [*b"CC", *b"HC", *b"CH"];
    let mut kinds: Vec<NonceKind> = vec![NonceKind::LegacyLowByte];
    for seq_be in [false, true] {
        for marker in MARKERS {
            kinds.push(NonceKind::V2 { seq_be, marker });
        }
        kinds.push(NonceKind::Legacy16Seq { seq_be });
    }

    for &nonce in &kinds {
        for key_rev in [false, true] {
            for tag_first in [true, false] {
                for aad in [Aad::None, Aad::Header4] {
                    let s = Scheme {
                        key_rev,
                        nonce,
                        tag_first,
                        aad,
                    };
                    if let Some(pt) = attempt(s) {
                        return Some((s, pt));
                    }
                }
            }
        }
    }
    None
}

/// Seal a host→client control message on the client's `detected` scheme, direction
/// flipped: V2 markers `H?` instead of `C?`; legacy keeps its construction with our
/// independent `seq`.
pub(super) fn encrypt_control(key: &[u8; 16], scheme: &Scheme, seq: u32, pt: &[u8]) -> Vec<u8> {
    let nonce = match scheme.nonce {
        NonceKind::V2 { seq_be, marker } => NonceKind::V2 {
            seq_be,
            marker: [b'H', marker[1]],
        },
        other => other,
    };
    seal_control(key, &Scheme { nonce, ..*scheme }, seq, pt)
}

/// Seal `pt` as `scheme` reads it: `[0x0001][length][seq][tag|ct per tag_first]`, AAD and
/// key order per the scheme. A client's packet is built the same way.
pub(super) fn seal_control(key: &[u8; 16], scheme: &Scheme, seq: u32, pt: &[u8]) -> Vec<u8> {
    let length = (4 + 16 + pt.len()) as u16;
    let mut wire = Vec::with_capacity(8 + 16 + pt.len());
    wire.extend_from_slice(&0x0001u16.to_le_bytes());
    wire.extend_from_slice(&length.to_le_bytes());
    wire.extend_from_slice(&seq.to_le_bytes());
    let aad: Vec<u8> = match scheme.aad {
        Aad::None => Vec::new(),
        Aad::Header4 => wire[0..4].to_vec(),
    };
    let ct_tag = gcm_seal(&scheme.key(key), &scheme.nonce.nonce(seq), pt, &aad);
    let (ct, tag) = ct_tag.split_at(ct_tag.len() - 16);
    if scheme.tag_first {
        wire.extend_from_slice(tag);
        wire.extend_from_slice(ct);
    } else {
        wire.extend_from_slice(ct);
        wire.extend_from_slice(tag);
    }
    wire
}

/// AES-128-GCM seal (companion to [`gcm_open`]); returns `ciphertext || tag`. Shared
/// with the RTSP plane under the same session key.
pub(super) fn gcm_seal(key: &[u8; 16], nonce: &[u8], pt: &[u8], aad: &[u8]) -> Vec<u8> {
    use aes_gcm::aead::consts::{U12, U16};
    use aes_gcm::aead::{Aead, KeyInit, Payload};
    use aes_gcm::{aes::Aes128, AesGcm};

    let p = Payload { msg: pt, aad };
    // Each arm's `try_into` is guarded by the length it matched on.
    match nonce.len() {
        12 => AesGcm::<Aes128, U12>::new_from_slice(key)
            .unwrap()
            .encrypt(nonce.try_into().expect("12-byte nonce"), p)
            .expect("GCM seal"),
        16 => AesGcm::<Aes128, U16>::new_from_slice(key)
            .unwrap()
            .encrypt(nonce.try_into().expect("16-byte nonce"), p)
            .expect("GCM seal"),
        _ => unreachable!("nonce length"),
    }
}

/// AES-128-GCM open, 12- or 16-byte nonce, explicit AAD. Plaintext iff the tag
/// authenticates. `ct_tag` is `ciphertext || tag` (aes-gcm's order).
pub(super) fn gcm_open(key: &[u8; 16], nonce: &[u8], ct_tag: &[u8], aad: &[u8]) -> Option<Vec<u8>> {
    use aes_gcm::aead::consts::{U12, U16};
    use aes_gcm::aead::{Aead, KeyInit, Payload};
    use aes_gcm::{aes::Aes128, AesGcm};

    let p = Payload { msg: ct_tag, aad };
    match nonce.len() {
        12 => AesGcm::<Aes128, U12>::new_from_slice(key)
            .ok()?
            .decrypt(nonce.try_into().ok()?, p)
            .ok(),
        16 => AesGcm::<Aes128, U16>::new_from_slice(key)
            .ok()?
            .decrypt(nonce.try_into().ok()?, p)
            .ok(),
        _ => None,
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    const KEY: [u8; 16] = [0x42; 16];

    /// `[u16 type LE][u16 length LE][body]`.
    pub(crate) fn msg(ty: u16, body: &[u8]) -> Vec<u8> {
        let mut v = ty.to_le_bytes().to_vec();
        v.extend_from_slice(&(body.len() as u16).to_le_bytes());
        v.extend_from_slice(body);
        v
    }

    /// 0x0301 invalidate-ref-frames: two LE `i64`, first and last lost frame.
    pub(crate) fn rfi(first: i64, last: i64) -> Vec<u8> {
        let mut b = first.to_le_bytes().to_vec();
        b.extend_from_slice(&last.to_le_bytes());
        msg(0x0301, &b)
    }

    /// Gen7 `IDX_LOSS_STATS` (0x0201): LE i32s lost, window 50 ms, 1000, last-good frame 7.
    pub(crate) fn loss(lost: i32) -> Vec<u8> {
        let mut b = Vec::new();
        for v in [lost, 50, 1000, 7] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        msg(0x0201, &b)
    }

    /// `reportFinalFrameFecStatus` layout, big-endian, 21 bytes.
    pub(crate) fn fec(total_data: u16, received_data: u16, received_parity: u16) -> Vec<u8> {
        let mut b = 7u32.to_be_bytes().to_vec();
        b.extend_from_slice(&[0; 6]);
        b.extend_from_slice(&total_data.to_be_bytes());
        b.extend_from_slice(&4u16.to_be_bytes());
        b.extend_from_slice(&received_data.to_be_bytes());
        b.extend_from_slice(&received_parity.to_be_bytes());
        b.extend_from_slice(&[20, 0, 1]);
        msg(0x5502, &b)
    }

    #[test]
    fn decodes_a_valid_rfi_range() {
        assert_eq!(decode_rfi_range(&rfi(40, 47)), Some((40, 47)));
        assert_eq!(decode_rfi_range(&rfi(5, 5)), Some((5, 5))); // single frame
    }

    #[test]
    fn rejects_short_or_nonsensical_ranges() {
        assert_eq!(decode_rfi_range(&[0x01, 0x03, 0x00, 0x00]), None); // header only, no body
        assert_eq!(decode_rfi_range(&rfi(-1, 9)), None); // negative first
        assert_eq!(decode_rfi_range(&rfi(9, 4)), None); // last < first
    }

    #[test]
    fn fec_status_counts_only_loss_parity_could_not_restore() {
        assert_eq!(decode_fec_status_loss(&fec(10, 8, 2)), Some(0), "recovered");
        assert_eq!(
            decode_fec_status_loss(&fec(10, 6, 1)),
            Some(3),
            "dropped frame"
        );
        assert_eq!(decode_fec_status_loss(&fec(10, 6, 1)[..24]), None, "short");
    }

    #[test]
    fn a_plaintext_is_classified_by_its_type_and_body() {
        use ControlMsg::*;
        let rows: Vec<(Vec<u8>, ControlMsg)> = vec![
            (vec![], Input),
            (vec![0x01], Input),
            (msg(0x0206, &[]), Input),
            (msg(0x0200, &[]), Input),
            (rfi(40, 47), InvalidateFrames(40, 47)),
            (rfi(9, 4), Keyframe(0x0301)),
            (msg(0x0301, &[]), Keyframe(0x0301)),
            (msg(0x0302, &[]), Keyframe(0x0302)),
            (msg(0x0305, &[1, 2]), Keyframe(0x0305)),
            (
                loss(3),
                LossStats {
                    lost: 3,
                    window_ms: 50,
                    last_good: 7,
                },
            ),
            (
                loss(-4),
                LossStats {
                    lost: 0,
                    window_ms: 50,
                    last_good: 7,
                },
            ),
            (msg(0x0201, &[1, 0, 0, 0]), Input),
            (fec(10, 6, 1), FecLoss(3)),
            (fec(10, 8, 2), FecLoss(0)),
            (msg(0x5502, &[0; 10]), Ignored),
        ];
        for (pt, want) in rows {
            assert_eq!(ControlMsg::parse(&pt), want, "{pt:02x?}");
        }
    }

    /// Every construction the sweep tries opens its own seal with the plaintext intact, and
    /// the scheme it locks reopens the next packet. Pins a gap: below seq 256 a 16-byte
    /// `seq` nonce (LE) is byte for byte the low-byte one, so the sweep locks the latter and
    /// that lock fails from seq 256 on. No known client sends the 16-byte form.
    #[test]
    fn every_sweep_scheme_opens_its_own_seal() {
        let pt = msg(0x0200, b"keepalive");
        let nonces = [
            NonceKind::LegacyLowByte,
            NonceKind::Legacy16Seq { seq_be: false },
            NonceKind::Legacy16Seq { seq_be: true },
            NonceKind::V2 {
                seq_be: false,
                marker: *b"CC",
            },
            NonceKind::V2 {
                seq_be: true,
                marker: *b"HC",
            },
            NonceKind::V2 {
                seq_be: false,
                marker: *b"CH",
            },
        ];
        const LAYOUTS: [(bool, bool, Aad); 8] = [
            (false, true, Aad::None),
            (false, true, Aad::Header4),
            (false, false, Aad::None),
            (false, false, Aad::Header4),
            (true, true, Aad::None),
            (true, true, Aad::Header4),
            (true, false, Aad::None),
            (true, false, Aad::Header4),
        ];
        for nonce in nonces {
            for (key_rev, tag_first, aad) in LAYOUTS {
                let s = Scheme {
                    key_rev,
                    nonce,
                    tag_first,
                    aad,
                };
                let (locked, opened) =
                    decrypt_control(&KEY, &seal_control(&KEY, &s, 300, &pt), &None)
                        .unwrap_or_else(|| panic!("{s:?}"));
                assert_eq!(opened, pt, "{s:?}");
                let next = seal_control(&KEY, &s, 301, &pt);
                assert!(
                    decrypt_control(&KEY, &next, &Some(locked)).is_some(),
                    "{s:?}"
                );
            }
        }
        let s = Scheme {
            key_rev: false,
            nonce: NonceKind::Legacy16Seq { seq_be: false },
            tag_first: true,
            aad: Aad::None,
        };
        let (locked, _) = decrypt_control(&KEY, &seal_control(&KEY, &s, 5, &pt), &None).unwrap();
        assert!(matches!(locked.nonce, NonceKind::LegacyLowByte));
        // pins suspected gap: the lock no longer opens this client once seq reaches 256
        assert!(decrypt_control(&KEY, &seal_control(&KEY, &s, 256, &pt), &Some(locked)).is_none());
    }
}
