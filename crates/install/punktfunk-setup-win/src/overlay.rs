//! The `[exe][compressed payload][footer]` sandwich (design D3), productionized from S2.
//!
//! Assemble BEFORE signing — Authenticode hashes the whole file including the overlay, so a
//! tampered payload breaks the signature. Reading afterwards must not assume the footer sits
//! at EOF: signing appends the certificate table there, so the reader takes the PE security
//! directory's file offset as the true end of the overlay (EOF when unsigned), and signing
//! pads to 8-byte alignment first, so the footer may sit up to 7 bytes before the table.
//!
//! Footer, 48 bytes, at overlay_end-48:
//!   [payload_len u64 LE][sha256(payload) 32B][magic "PFSETUP\x01" 8B]

use sha2::{Digest, Sha256};

pub const MAGIC: &[u8; 8] = b"PFSETUP\x01";
pub const FOOTER_LEN: usize = 8 + 32 + 8;

/// The certificate table's FILE offset from the PE optional header, 0 when unsigned.
pub fn cert_table_offset(data: &[u8]) -> Option<u32> {
    let e_lfanew = u32::from_le_bytes(data.get(0x3c..0x40)?.try_into().ok()?) as usize;
    if data.get(e_lfanew..e_lfanew + 4)? != b"PE\0\0" {
        return None;
    }
    let opt = e_lfanew + 24;
    let magic = u16::from_le_bytes(data.get(opt..opt + 2)?.try_into().ok()?);
    // Security directory = data directory index 4; PE32+ vs PE32 shifts the table.
    let directories = opt + if magic == 0x20b { 112 } else { 96 };
    let entry = directories + 4 * 8;
    Some(u32::from_le_bytes(
        data.get(entry..entry + 4)?.try_into().ok()?,
    ))
}

/// Where the overlay ends: before the certificate table when signed, EOF when not.
pub fn overlay_end(data: &[u8]) -> usize {
    match cert_table_offset(data) {
        Some(off) if off != 0 && (off as usize) <= data.len() => off as usize,
        _ => data.len(),
    }
}

/// `exe + payload + footer`, ready for signtool.
pub fn assemble(exe: &[u8], payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(exe.len() + payload.len() + FOOTER_LEN);
    out.extend_from_slice(exe);
    out.extend_from_slice(payload);
    out.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    out.extend_from_slice(&Sha256::digest(payload));
    out.extend_from_slice(MAGIC);
    out
}

/// Locate and sha-verify the payload inside an (optionally signed) assembled exe.
pub fn extract(data: &[u8]) -> Result<&[u8], String> {
    let table = overlay_end(data);
    let end = (0..8)
        .filter_map(|pad| table.checked_sub(pad))
        .find(|&e| e >= FOOTER_LEN && &data[e - 8..e] == MAGIC)
        .ok_or("no payload footer before the certificate table")?;
    let footer = &data[end - FOOTER_LEN..end];
    let len = u64::from_le_bytes(footer[0..8].try_into().unwrap()) as usize;
    let start = (end - FOOTER_LEN)
        .checked_sub(len)
        .ok_or("footer names a payload longer than the file")?;
    let payload = &data[start..end - FOOTER_LEN];
    let digest = Sha256::digest(payload);
    if digest.as_slice() != &footer[8..40] {
        return Err("payload sha256 does not match the footer".into());
    }
    Ok(payload)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A minimal PE32+ skeleton: DOS header pointing at a PE header whose security directory
    /// entry lives where the parser looks. Enough structure for offset arithmetic, no more.
    pub(crate) fn fake_pe(cert_offset: u32, tail: usize) -> Vec<u8> {
        let e_lfanew = 0x80usize;
        let mut d = vec![0u8; e_lfanew + 24 + 112 + 4 * 8 + 8 + tail];
        d[0x3c..0x40].copy_from_slice(&(e_lfanew as u32).to_le_bytes());
        d[e_lfanew..e_lfanew + 4].copy_from_slice(b"PE\0\0");
        let opt = e_lfanew + 24;
        d[opt..opt + 2].copy_from_slice(&0x20bu16.to_le_bytes());
        let entry = opt + 112 + 4 * 8;
        d[entry..entry + 4].copy_from_slice(&cert_offset.to_le_bytes());
        d
    }

    #[test]
    fn round_trip_unsigned_and_behind_a_cert_table() {
        let exe = fake_pe(0, 16);
        let payload = b"the compressed archive".to_vec();
        let mut assembled = assemble(&exe, &payload);
        assert_eq!(extract(&assembled).unwrap(), payload.as_slice());

        // "Sign" it the way real tools do: pad to 8-byte alignment, stamp the security
        // directory with the padded offset, append a fake certificate table. The footer is
        // neither at EOF nor flush against the table and must still be found.
        while !assembled.len().is_multiple_of(8) {
            assembled.push(0);
        }
        let cert_at = assembled.len() as u32;
        let entry = 0x80 + 24 + 112 + 4 * 8;
        assembled[entry..entry + 4].copy_from_slice(&cert_at.to_le_bytes());
        assembled.extend_from_slice(&[0xAB; 64]);
        assert_eq!(extract(&assembled).unwrap(), payload.as_slice());
    }

    #[test]
    fn a_tampered_payload_fails_the_footer_hash() {
        let exe = fake_pe(0, 16);
        let mut assembled = assemble(&exe, b"payload bytes");
        let at = assembled.len() - FOOTER_LEN - 3;
        assembled[at] ^= 0xFF;
        assert!(extract(&assembled).unwrap_err().contains("sha256"));
    }

    #[test]
    fn a_plain_exe_reports_no_footer() {
        let exe = fake_pe(0, 64);
        assert!(extract(&exe).unwrap_err().contains("footer"));
    }
}
