//! What the native encoder's hardware tests share: the moving picture and the loss they
//! recover from.

use pf_libva::encode::Encoder;

/// A moving edge, so successive frames actually differ — an encoder fed identical
/// pictures can emit almost nothing and still look like it works.
pub fn frame(width: usize, height: usize, phase: usize) -> (Vec<u8>, Vec<u8>) {
    let mut y = vec![16u8; width * height];
    for (row, line) in y.chunks_mut(width).enumerate() {
        for (col, px) in line.iter_mut().enumerate() {
            if (col + phase * 8) % 64 < 32 || row % 48 < 8 {
                *px = 235;
            }
        }
    }
    (y, vec![128u8; width * height / 2])
}

/// Pictures 0..13 with 8 and 9 lost: after 9, every slot from 8 on is distrusted and 10 is
/// predicted from the newest pre-loss slot, which must hold 7. Returns every access unit in
/// wire order; the caller drops 8 and 9 as the client would.
pub fn encode_through_a_loss(enc: &mut Encoder, width: usize, height: usize) -> Vec<Vec<u8>> {
    let encode = |enc: &mut Encoder, i: usize, anchor: Option<usize>| {
        let (y, uv) = frame(width, height, i);
        enc.write_nv12(&y, &uv).expect("fill");
        match anchor {
            Some(slot) => enc.encode_anchored(slot).expect("anchored encode"),
            None => enc.encode(i == 0).expect("encode"),
        }
        let pic = enc
            .collect(true)
            .expect("collect")
            .expect("a picture per encode");
        assert_eq!(pic.is_idr, i == 0, "picture {i}");
        assert_eq!(pic.recovery_anchor, anchor.is_some());
        assert_eq!(pic.wire, i as i64);
        pic.bytes
    };
    let mut aus = Vec::new();
    for i in 0..10 {
        aus.push(encode(enc, i, None));
    }
    // `plan_slot_recovery` on the session's slots: taint everything from 8 on, anchor on
    // the newest before it.
    let refs = enc.slots();
    let tainted = refs
        .iter()
        .filter(|&&(_, wire)| wire >= 8)
        .fold(0u32, |m, &(slot, _)| m | 1 << slot);
    let (anchor, anchor_wire) = refs
        .iter()
        .copied()
        .filter(|&(_, wire)| wire < 8)
        .max_by_key(|&(_, wire)| wire)
        .expect("a pre-loss slot survives");
    assert_eq!(anchor_wire, 7);
    enc.distrust(tainted);
    aus.push(encode(enc, 10, Some(anchor)));
    for i in 11..13 {
        aus.push(encode(enc, i, None));
    }
    aus
}
