//! The AMD video chip's firmware version, and which colour conversions its RGB front end
//! (EFC) gets right.
//!
//! VCN 5 firmware before 9.13.38 converts BT.709 narrow and BT.2020 full and gets the other
//! two pairs wrong. A driver's two masks cannot name two pairs, so RADV offers 709 narrow
//! alone there, and hands whatever pair a session names to the firmware unchecked. The
//! version comes from the kernel, decoded as Mesa's `ac_gpu_info.c` decodes it.
//!
//! Pure but for [`query`], so the table runs on every target.

/// One VCN's generation and firmware version.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VcnFirmware {
    /// Hardware generation: 5 on an RX 9000.
    pub ip_major: u32,
    pub dec: u32,
    pub enc_minor: u32,
    pub revision: u32,
}

/// The `(model, range)` pairs an EFC converts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum EfcPairs {
    /// The pairs the driver's masks span.
    #[default]
    AsReported,
    /// BT.709 narrow and BT.2020 full, nothing else.
    TwoPairs,
}

impl VcnFirmware {
    /// The first firmware whose EFC converts every pair on VCN 5 (Mesa's `have_efc_range_bug`).
    pub const EFC_FIXED: (u32, u32, u32) = (9, 13, 38);

    /// `ver` is `AMDGPU_INFO_FW_VCN`'s.
    pub(crate) fn decode(ip_major: u32, ver: u32) -> Self {
        VcnFirmware {
            ip_major,
            dec: (ver >> 24) & 0xf,
            enc_minor: (ver >> 12) & 0xff,
            revision: ver & 0xfff,
        }
    }

    /// This firmware gets two of the four pairs wrong.
    pub fn efc_two_pairs(self) -> bool {
        let (dec, enc, rev) = Self::EFC_FIXED;
        let fixed = self.dec > dec
            || self.enc_minor > enc
            || (self.dec == dec && self.enc_minor == enc && self.revision >= rev);
        self.ip_major >= 5 && !fixed
    }
}

impl std::fmt::Display for VcnFirmware {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.dec, self.enc_minor, self.revision)
    }
}

/// What an EFC converts, from RADV's `offered` masks and the chip's firmware. `narrow_709` is
/// the `(model, range)` mask pair naming 709 narrow alone: RADV offers exactly that on VCN 5
/// where it cannot tell, or will not say, that 2020 full works too.
pub(crate) fn efc_pairs(
    radv: bool,
    firmware: Option<VcnFirmware>,
    offered: (u32, u32),
    narrow_709: (u32, u32),
) -> EfcPairs {
    let old = firmware.is_some_and(VcnFirmware::efc_two_pairs);
    if radv && (old || offered == narrow_709) {
        EfcPairs::TwoPairs
    } else {
        EfcPairs::AsReported
    }
}

/// `node`'s VCN generation and firmware, `None` off amdgpu or without an encode VCN.
#[cfg(target_os = "linux")]
pub(crate) fn query(node: &std::path::Path) -> Option<VcnFirmware> {
    use std::os::fd::AsRawFd;

    /// `struct drm_amdgpu_info` with its `query_hw_ip` / `query_fw` arms, `<drm/amdgpu_drm.h>`.
    #[repr(C)]
    struct Info {
        return_pointer: u64,
        return_size: u32,
        query: u32,
        arg: [u32; 4],
    }
    /// `DRM_IOW(DRM_COMMAND_BASE + DRM_AMDGPU_INFO, struct drm_amdgpu_info)`.
    const AMDGPU_INFO: u64 = (1 << 30) | (32 << 16) | (0x64 << 8) | 0x45;
    const HW_IP_INFO: u32 = 0x02;
    const HW_IP_VCN_ENC: u32 = 7;
    const FW_VERSION: u32 = 0x0e;
    const FW_VCN: u32 = 0x0e;

    let file = std::fs::File::open(node).ok()?;
    let ask = |query: u32, arg: [u32; 4], out: &mut [u32]| {
        let mut info = Info {
            return_pointer: out.as_mut_ptr() as u64,
            return_size: std::mem::size_of_val(out) as u32,
            query,
            arg,
        };
        // SAFETY: `info` is the struct the ioctl reads, and the kernel writes at most
        // `return_size` bytes to `out`, which outlives the call.
        unsafe { libc::ioctl(file.as_raw_fd(), AMDGPU_INFO as _, &mut info) == 0 }
    };
    // `drm_amdgpu_info_hw_ip`: word 0 the block's major, word 7 `ip_discovery_version`
    // (bits 23:16 major, zero on a kernel that predates it).
    let mut ip = [0u32; 8];
    if !ask(HW_IP_INFO, [HW_IP_VCN_ENC, 0, 0, 0], &mut ip) {
        return None;
    }
    let ip_major = match (ip[7] >> 16) & 0xff {
        0 => ip[0],
        major => major,
    };
    // `drm_amdgpu_info_firmware { ver, feature }`.
    let mut fw = [0u32; 2];
    if !ask(FW_VERSION, [FW_VCN, 0, 0, 0], &mut fw) || fw[0] == 0 {
        return None;
    }
    Some(VcnFirmware::decode(ip_major, fw[0]))
}

#[cfg(test)]
mod tests {
    use super::*;

    const NARROW_709: (u32, u32) = (0x04, 0x02);
    const ALL: (u32, u32) = (0x14, 0x03);

    /// The kernel's boot line for these words: "ENC: 1.24 DEC: 9 VEP: 0 Revision: 34" and
    /// "ENC: 1.13 DEC: 9 VEP: 0 Revision: 33".
    #[test]
    fn a_firmware_word_decodes_as_the_kernel_prints_it() {
        let vcn4 = VcnFirmware::decode(4, 0x0911_8022);
        assert_eq!((vcn4.dec, vcn4.enc_minor, vcn4.revision), (9, 24, 34));
        assert_eq!(VcnFirmware::decode(5, 0x0910_d021).to_string(), "9.13.33");
    }

    /// Mesa's `check_vcn_fw_version(info, 9, 13, 38)`, on VCN 5 and up only.
    #[test]
    fn only_vcn_5_firmware_before_9_13_38_converts_two_pairs() {
        let fw = |ip, dec, enc, rev| VcnFirmware {
            ip_major: ip,
            dec,
            enc_minor: enc,
            revision: rev,
        };
        assert!(fw(5, 9, 13, 33).efc_two_pairs());
        assert!(fw(5, 9, 13, 37).efc_two_pairs());
        assert!(fw(5, 9, 12, 99).efc_two_pairs());
        assert!(!fw(5, 9, 13, 38).efc_two_pairs());
        assert!(!fw(5, 9, 14, 0).efc_two_pairs());
        assert!(!fw(5, 10, 0, 0).efc_two_pairs());
        assert!(!fw(4, 9, 13, 33).efc_two_pairs());
        assert!(fw(6, 9, 13, 33).efc_two_pairs());
    }

    /// Old firmware decides whatever the driver offers, and RADV's 709-narrow-only offer
    /// decides whatever the firmware. No other driver's offer is read that way.
    #[test]
    fn two_pairs_follow_the_firmware_or_the_offer() {
        let old = Some(VcnFirmware::decode(5, 0x0910_d021));
        let fixed = Some(VcnFirmware::decode(5, 0x0910_d026));
        let vcn4 = Some(VcnFirmware::decode(4, 0x0911_8022));
        for (radv, fw, offered, want) in [
            (true, old, NARROW_709, EfcPairs::TwoPairs),
            (true, old, ALL, EfcPairs::TwoPairs),
            (true, fixed, NARROW_709, EfcPairs::TwoPairs),
            (true, None, NARROW_709, EfcPairs::TwoPairs),
            (true, fixed, ALL, EfcPairs::AsReported),
            (true, vcn4, ALL, EfcPairs::AsReported),
            (true, None, ALL, EfcPairs::AsReported),
            (false, old, NARROW_709, EfcPairs::AsReported),
        ] {
            assert_eq!(
                efc_pairs(radv, fw, offered, NARROW_709),
                want,
                "{radv} {fw:?} {offered:?}"
            );
        }
    }
}
