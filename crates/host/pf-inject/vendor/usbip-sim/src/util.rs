/// Check validity of a USB descriptor
pub fn verify_descriptor(desc: &[u8]) {
    let mut offset = 0;
    while offset < desc.len() {
        offset += desc[offset] as usize; // length
    }
    assert_eq!(offset, desc.len());
}

// (In-crate test module removed in the vendored copy — see NOTICE.)
