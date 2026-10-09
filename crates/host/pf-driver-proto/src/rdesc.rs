//! Test support: what a HID report descriptor declares, walked the way a HID parser does.
//! hidclass sizes its buffers from these lengths and refuses a longer completion.

use alloc::collections::BTreeMap;

/// Main-item tags (`prefix & 0xFC`) that key [`report_lens`].
pub const INPUT: u8 = 0x80;
pub const OUTPUT: u8 = 0x90;
pub const FEATURE: u8 = 0xB0;

/// Bytes per `(main-item tag, report id)`, id byte included. Global items persist and Push/Pop
/// save and restore them; each main item adds size × count bits. An undeclared report has no
/// entry. Panics on a report that is not whole bytes or a Pop without a Push.
pub fn report_lens(d: &[u8]) -> BTreeMap<(u8, u8), usize> {
    let (mut size, mut count, mut id) = (0u32, 0u32, 0u8);
    let mut stack = alloc::vec::Vec::new();
    let mut bits = BTreeMap::<(u8, u8), u32>::new();
    let mut i = 0;
    while i < d.len() {
        let prefix = d[i];
        let n = [0, 1, 2, 4][usize::from(prefix & 3)];
        let mut v = 0u32;
        for k in 0..n {
            v |= u32::from(d[i + 1 + k]) << (8 * k);
        }
        match prefix & 0xFC {
            0x74 => size = v,
            0x94 => count = v,
            0x84 => id = v as u8,
            0xA4 => stack.push((size, count, id)),
            0xB4 => (size, count, id) = stack.pop().expect("HID Pop without a Push"),
            tag @ (INPUT | OUTPUT | FEATURE) => *bits.entry((tag, id)).or_default() += size * count,
            _ => {}
        }
        i += 1 + n;
    }
    bits.into_iter()
        .map(|((tag, id), b)| {
            assert!(b % 8 == 0, "report {tag:#04x}/{id:#04x} is not whole bytes");
            ((tag, id), 1 + b as usize / 8)
        })
        .collect()
}
