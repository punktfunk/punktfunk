//! What every decode rung answers to [`Decoder`](crate::video::Decoder), and the six
//! decoders that answer it.
//!
//! A hardware rung reports stream damage as `Ok(None)` plus
//! [`Rung::take_recovery_request`]; its `Err` is a decoder fault and counts toward
//! demotion. The CPU rung and PyroWave have nothing below them, so their `Err`
//! goes to the pump. Only the Vulkan rung has a timeline; the defaults answer for
//! the others.

use anyhow::Result;
use punktfunk_core::au::{AuBuf, AuMemory};
use std::sync::Arc;

use crate::video::DecodedImage;
use crate::video_caps::NativeRung;
use crate::video_software::SoftwareDecoder;
use crate::video_types::DecodeHealth;
use crate::video_vk_native::NativeVulkanDecoder;

pub(crate) trait Rung {
    /// The ladder rung this is. `None` for PyroWave: no ladder holds it.
    fn id(&self) -> Option<NativeRung>;

    /// The decoder as the init log names it.
    fn name(&self) -> &'static str;

    /// Feed one access unit. `user_flags` and `complete` are wire facts only PyroWave
    /// reads; every other rung takes whole units.
    fn decode(
        &mut self,
        au: &[u8],
        user_flags: u32,
        complete: bool,
    ) -> Result<Option<DecodedImage>>;

    /// [`Self::decode`] with the AU's own buffer, for a rung that reads it where it lies.
    fn decode_au(
        &mut self,
        au: AuBuf,
        user_flags: u32,
        complete: bool,
    ) -> Result<Option<DecodedImage>> {
        self.decode(&au, user_flags, complete)
    }

    /// Memory this rung reads AUs from in place, for the session to lend; `None` = the
    /// heap. A resize can replace it.
    fn au_memory(&self) -> Option<Arc<dyn AuMemory>> {
        None
    }

    /// Drain the re-anchor ask the last `decode` raised by concealing damage.
    fn take_recovery_request(&mut self) -> bool {
        false
    }

    /// Decode-integrity counters. `None` is "cannot see corruption"; `Some(default)`
    /// is "looked and saw none".
    fn health(&self) -> Option<DecodeHealth> {
        None
    }

    /// Drop the planner's damaged-chain marks. A rung with no planner has none.
    fn forgive_unclean(&mut self) {}

    /// Wait for a decode on the rung's timeline. `false` declines the sample.
    fn wait_timeline(&mut self, _sem: u64, _value: u64, _timeout_ns: u64) -> bool {
        false
    }

    /// Whether a timeline decode is complete now. `true` where nothing is pending.
    fn timeline_done(&mut self, _sem: u64, _value: u64) -> bool {
        true
    }

    /// The timeline wait is also the media clock boost (Intel on i915).
    fn boosted(&self) -> bool {
        false
    }

    /// Run `wake` once each shipped decode completes, off the pump's thread. A rung that
    /// returns with the pixels done has nothing to wake for.
    fn set_decode_wake(&mut self, _wake: std::sync::Arc<dyn Fn() + Send + Sync>) {}

    /// Newest planned decode-order ordinal. 0 on a rung that stamps none.
    fn decode_order(&self) -> u64 {
        0
    }
}

impl Rung for NativeVulkanDecoder {
    fn id(&self) -> Option<NativeRung> {
        Some(NativeRung::Vulkan)
    }

    fn name(&self) -> &'static str {
        NativeRung::Vulkan.name()
    }

    fn decode(&mut self, au: &[u8], _: u32, _: bool) -> Result<Option<DecodedImage>> {
        Ok(Self::decode(self, au)?.map(DecodedImage::NativeVk))
    }

    fn take_recovery_request(&mut self) -> bool {
        Self::take_recovery_request(self)
    }

    fn health(&self) -> Option<DecodeHealth> {
        Some(Self::health(self))
    }

    fn forgive_unclean(&mut self) {
        Self::forgive_unclean(self);
    }

    fn wait_timeline(&mut self, sem: u64, value: u64, timeout_ns: u64) -> bool {
        Self::wait_timeline(self, sem, value, timeout_ns)
    }

    fn timeline_done(&mut self, sem: u64, value: u64) -> bool {
        Self::timeline_done(self, sem, value)
    }

    fn boosted(&self) -> bool {
        Self::boosted(self)
    }

    fn set_decode_wake(&mut self, wake: std::sync::Arc<dyn Fn() + Send + Sync>) {
        Self::set_decode_wake(self, wake);
    }

    fn decode_order(&self) -> u64 {
        Self::decode_order(self)
    }
}

// This rung runs no per-picture decode-status query: `health` reports concealment and
// refusals from the planner, and `failed` stays 0.
#[cfg(target_os = "linux")]
impl Rung for crate::video_vaapi_native::NativeVaapiDecoder {
    fn id(&self) -> Option<NativeRung> {
        Some(NativeRung::Vaapi)
    }

    fn name(&self) -> &'static str {
        Self::name(self)
    }

    fn decode(&mut self, au: &[u8], _: u32, _: bool) -> Result<Option<DecodedImage>> {
        Ok(Self::decode(self, au)?.map(DecodedImage::NativeDmabuf))
    }

    fn take_recovery_request(&mut self) -> bool {
        Self::take_recovery_request(self)
    }

    fn health(&self) -> Option<DecodeHealth> {
        Some(Self::health(self))
    }

    fn forgive_unclean(&mut self) {
        Self::forgive_unclean(self);
    }
}

#[cfg(target_os = "linux")]
impl Rung for crate::video_v4l2::NativeV4l2Decoder {
    fn id(&self) -> Option<NativeRung> {
        Some(NativeRung::V4l2)
    }

    fn name(&self) -> &'static str {
        Self::name(self)
    }

    fn decode(&mut self, au: &[u8], _: u32, _: bool) -> Result<Option<DecodedImage>> {
        Self::decode(self, au)
    }

    fn take_recovery_request(&mut self) -> bool {
        Self::take_recovery_request(self)
    }

    fn health(&self) -> Option<DecodeHealth> {
        Some(Self::health(self))
    }

    fn forgive_unclean(&mut self) {
        Self::forgive_unclean(self);
    }
}

// This rung reads no per-picture status either: `health` is the planner's view.
#[cfg(windows)]
impl Rung for crate::video_d3d11_native::NativeD3d11Decoder {
    fn id(&self) -> Option<NativeRung> {
        Some(NativeRung::D3d11va)
    }

    fn name(&self) -> &'static str {
        Self::name(self)
    }

    fn decode(&mut self, au: &[u8], _: u32, _: bool) -> Result<Option<DecodedImage>> {
        Ok(Self::decode(self, au)?.map(DecodedImage::D3d11))
    }

    fn take_recovery_request(&mut self) -> bool {
        Self::take_recovery_request(self)
    }

    fn health(&self) -> Option<DecodeHealth> {
        Some(Self::health(self))
    }

    fn forgive_unclean(&mut self) {
        Self::forgive_unclean(self);
    }
}

// Nothing else decodes PyroWave: a failure renegotiates the codec.
#[cfg(feature = "pyrowave")]
impl Rung for crate::video_pyrowave::PyroWaveDecoder {
    fn id(&self) -> Option<NativeRung> {
        None
    }

    fn name(&self) -> &'static str {
        "pyrowave"
    }

    fn decode(
        &mut self,
        au: &[u8],
        user_flags: u32,
        complete: bool,
    ) -> Result<Option<DecodedImage>> {
        let aligned = user_flags & punktfunk_core::packet::USER_FLAG_CHUNK_ALIGNED != 0;
        let frame = self.decode_frame(au, aligned, complete)?;
        // The presenter and the export lanes read on their own terms: hand over done planes.
        if let Some(f) = &frame {
            self.wait_ready(f)?;
        }
        Ok(frame.map(DecodedImage::PyroWave))
    }

    fn decode_au(
        &mut self,
        au: AuBuf,
        user_flags: u32,
        complete: bool,
    ) -> Result<Option<DecodedImage>> {
        let aligned = user_flags & punktfunk_core::packet::USER_FLAG_CHUNK_ALIGNED != 0;
        let frame = Self::decode_au(self, au, aligned, complete)?;
        if let Some(f) = &frame {
            self.wait_ready(f)?;
        }
        Ok(frame.map(DecodedImage::PyroWave))
    }

    fn au_memory(&self) -> Option<Arc<dyn AuMemory>> {
        Self::au_memory(self)
    }
}

// Last in every ladder, and the only rung that can fail to exist for a codec:
// `last_rung_verdict` answers that.
impl Rung for SoftwareDecoder {
    fn id(&self) -> Option<NativeRung> {
        Some(NativeRung::Software)
    }

    fn name(&self) -> &'static str {
        NativeRung::Software.name()
    }

    fn decode(&mut self, au: &[u8], _: u32, _: bool) -> Result<Option<DecodedImage>> {
        Ok(Self::decode(self, au)?.map(DecodedImage::Cpu))
    }
}
