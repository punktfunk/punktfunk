//! Decode counters, the Welcome picture shape, and the DXGI driver version.
//!
//! `video` re-exports these, so existing paths stay. `d3d11va` names
//! them without building the Vulkan ladder. `ColorDesc` stays in `video_color`.

/// Session-cumulative decode integrity. Only a native rung fills one.
/// Counters are monotonic; the stats window diffs them like `frames_dropped`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DecodeHealth {
    /// AUs whose plan needed concealment (missing DPB ref, `frame_num` gap,
    /// truncated NALU walk). Output was released unshown.
    pub damaged: u64,
    /// Frames the driver reported corrupt (`RESULT_STATUS_ONLY`). Distinct from
    /// [`Self::damaged`]: damaged is an incomplete bitstream; failed is hardware
    /// that could not decode what arrived. Structurally 0 where
    /// [`Self::status_queries`] is false — [`Self::note`] still extends [`Self::run`].
    pub failed: u64,
    /// AUs the decoder refused (plan error or session failure). No picture.
    /// Distinct from [`Self::damaged`]: concealment coped; refusal could not run.
    pub refused: u64,
    /// Consecutive AUs with no showable picture; 0 on the next clean AU.
    /// Separates a recovering lossy link (`run 0`) from a stream that went down.
    pub run: u32,
    /// Longest [`Self::run`] of the session. A 1 Hz sample of `run` misses the peak.
    pub worst_run: u32,
    /// Correctly decoded frames discarded because the deliverable queue overflowed.
    /// Not damaged/refused/failed: the stream was fine and a picture showed, so
    /// this must not extend [`Self::run`]. Structurally 0 without a deliverable queue.
    pub dropped: u64,
    /// Per-op decode-status queries (`queryResultStatusSupport`). False on RADV
    /// (a query hangs the VCN ring); [`Self::failed`] then stays 0. Distinguishes
    /// clean from unmeasured.
    pub status_queries: bool,
}

impl DecodeHealth {
    /// Fold one AU's verdict. Damaged, refused, and driver-failed all extend the
    /// run. Where [`Self::status_queries`] is false, a `Failed` read still extends
    /// the run but does not count as [`Self::failed`].
    pub(crate) fn note(&mut self, damaged: bool, refused: bool, failed: u32) {
        if self.status_queries {
            self.failed = self.failed.saturating_add(u64::from(failed));
        }
        if damaged {
            self.damaged = self.damaged.saturating_add(1);
        }
        if refused {
            self.refused = self.refused.saturating_add(1);
        }
        if damaged || refused || failed > 0 {
            self.run = self.run.saturating_add(1);
            self.worst_run = self.worst_run.max(self.run);
        } else {
            self.run = 0;
        }
    }

    /// One correctly-decoded frame discarded unshown. Separate from [`Self::note`]:
    /// several frames can drop inside one AU that still shipped a picture. Never touches [`Self::run`].
    #[cfg(feature = "desktop")]
    pub(crate) fn note_dropped(&mut self) {
        self.dropped = self.dropped.saturating_add(1);
    }
}

/// Picture shape the host resolved in Welcome, before any AU arrives.
///
/// The in-band SPS stays authoritative. Available at construction so a
/// device-dependent shape refuses before the rung is chosen, instead of
/// demoting past it on the first decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamFormat {
    /// [`punktfunk_core::quic::CHROMA_IDC_420`] (1) or
    /// [`punktfunk_core::quic::CHROMA_IDC_444`] (3). A host that omits it reads as 4:2:0, never 0.
    pub chroma_format_idc: u8,
    /// Bits per component: 8, or 10 for Main10/HDR. A host that omits it reads 8.
    pub bit_depth: u8,
}

impl StreamFormat {
    /// 8-bit 4:2:0 — every H.264 session, and what an omitted Welcome shape decodes to.
    pub const SDR_420_8: StreamFormat = StreamFormat {
        chroma_format_idc: punktfunk_core::quic::CHROMA_IDC_420,
        bit_depth: 8,
    };

    /// `bit_depth` as H.265 `bit_depth_luma_minus8`. `None` outside 8/10 is a refusal, not a skipped probe.
    #[cfg(feature = "desktop")]
    pub(crate) fn bit_depth_minus8(self) -> Option<u8> {
        self.bit_depth.checked_sub(8)
    }
}

/// DXGI's packed user-mode driver version (`CheckInterfaceSupport`) as the four
/// Device Manager fields: `32.0.21025.10016` becomes `[32, 0, 21025, 10016]`.
pub fn umd_version_parts(raw: i64) -> [u16; 4] {
    let v = raw as u64;
    [
        (v >> 48) as u16,
        (v >> 32) as u16,
        (v >> 16) as u16,
        v as u16,
    ]
}
