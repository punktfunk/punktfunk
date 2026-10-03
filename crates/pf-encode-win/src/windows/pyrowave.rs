//! Windows PyroWave host encoder: separate-plane zero-copy D3D11→Vulkan through
//! pyrowave's own compat device (`design/pyrowave-windows-host-zerocopy.md`).
//! Intra-only wavelet; Windows twin of `enc/linux/pyrowave.rs`.
//!
//! Pyrowave owns the Vulkan device, selected by the render GPU's vendor/device-id
//! (`pyrowave_create_device_by_compat`). Capture CSC writes two shareable D3D11
//! planes — full-res R8 Y + half-res R8G8 CbCr, BT.709 limited — imported as NT
//! handles. Separate single/two-component textures import at any size; a planar
//! NV12 does not. A shared D3D11 fence imported as a Vulkan timeline orders the
//! wavelet read after convert. `encode_gpu_synchronous` acquire/encode/release
//! in one pyrowave submission (`VK_QUEUE_FAMILY_EXTERNAL`). Every AU is a
//! keyframe; framing is [`crate::pyrowave_wire`].
//!
//! Capture: `pf-capture` `windows/idd_push.rs`. Y on `D3d11Frame::texture`,
//! CbCr + fence on `D3d11Frame::pyro`.
// `unsafe_op_in_unsafe_fn` off: this file is pyrowave-sys + D3D11/Vulkan interop.
// Clearing it means deleting markers with no caller contract, not wrapping each call.
#![allow(unsafe_op_in_unsafe_fn)]

// Every `unsafe` block in this module carries a `// SAFETY:` proof (crate root enforces it).

use crate::pyrowave_ffi::{packetize, pw_check};
use crate::pyrowave_wire::{self, AuStream, FrameBudget};
use crate::{EncodedFrame, Encoder, EncoderCaps};
use anyhow::{bail, Context, Result};
use pf_frame::{CapturedFrame, FramePayload};
use pyrowave_sys as pw;
use std::collections::VecDeque;
use windows::core::{Interface, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, DuplicateHandle, DUPLICATE_SAME_ACCESS, HANDLE};
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::Graphics::Dxgi::IDXGIResource1;
use windows::Win32::System::Threading::GetCurrentProcess;

/// Headroom over the per-frame rate budget for block headers + meta.
const BS_SLACK: usize = 256 * 1024;
/// Import-cache cap. IDD out-ring is OUT_RING=3; growth past that is a mid-life
/// ring recreate, after which stale imports must be evicted.
const IMPORT_CACHE_CAP: usize = 8;

/// Texture COM address plus the extent it was imported at (COM pointers recycle).
type PlaneKey = (isize, u32, u32);

// Vulkan #define / flags not emitted by pyrowave-sys bindgen (only C-API-reachable
// enums are). bindgen aliases those as `u32`, so these spec literals assign as-is.
const VK_IMAGE_USAGE_TRANSFER_SRC_BIT: u32 = 0x0000_0001;
const VK_IMAGE_USAGE_TRANSFER_DST_BIT: u32 = 0x0000_0002;
const VK_IMAGE_USAGE_SAMPLED_BIT: u32 = 0x0000_0004;
/// `VK_QUEUE_FAMILY_EXTERNAL` (`~0u32 - 1`): D3D11 owns the image; pyrowave
/// acquire/release transitions across the interop boundary.
const VK_QUEUE_FAMILY_EXTERNAL: u32 = 0xFFFF_FFFE;

// Do not raise GPU scheduling here. `pf-frame`'s `dxgi::elevate_gpu_priority_of` owns
// that process-wide (`PUNKTFUNK_GPU_PRIORITY_CLASS`); a second owner races it.

pub struct PyroWaveEncoder {
    pw_dev: pw::pyrowave_device,
    pw_enc: pw::pyrowave_encoder,
    // Vulkan timeline alias of the capturer's D3D11 fence; null until first-frame import.
    sync: pw::pyrowave_sync_object,
    /// Capturer ring generation the cached plane imports belong to. A recreate
    /// bumps it; COM addresses recycle, so identity cannot rest on the pointer.
    ring_gen: Option<u32>,
    y_images: Vec<(PlaneKey, pw::pyrowave_image)>,
    cbcr_images: Vec<(PlaneKey, pw::pyrowave_image)>,

    width: u32,
    height: u32,
    /// 4:4:4 = full-res CbCr plane + `Chroma444` pyrowave objects.
    chroma444: bool,
    /// Depth ≥10: the FP16 CSC writes P010-style studio codes into 16-bit UNORM planes.
    ten_bit: bool,
    /// The sequence header says BT.2020/PQ; otherwise BT.709. Implies `ten_bit`.
    pq: bool,
    budget: FrameBudget,
    /// Boundary and streamed-AU cursor. Encode is synchronous, so an AU is complete before
    /// its first chunk leaves.
    stream: AuStream,
    bitstream: Vec<u8>,
    pending: VecDeque<EncodedFrame>,
}

// SAFETY: encode thread only; pyrowave handles are owned and only touched from
// that thread. Pyrowave submits GPU work only inside the API calls we make.
// D3D11 texture pointers travel as `isize` cache keys, never dereferenced here.
unsafe impl Send for PyroWaveEncoder {}

impl PyroWaveEncoder {
    // The adapter hint adds two ids to the mode tuple; a struct for one caller buys nothing.
    #[allow(clippy::too_many_arguments)]
    pub fn open(
        width: u32,
        height: u32,
        fps: u32,
        bitrate_bps: u64,
        chroma: crate::ChromaFormat,
        bit_depth: u8,
        // BT.2020 PQ; a 10-bit session without it is SDR wide colour, BT.709.
        hdr: bool,
        // PCI ids of the selected render adapter; `(0, 0)` lets pyrowave pick.
        vendor_id: u32,
        device_id: u32,
    ) -> Result<Self> {
        let chroma444 = chroma.is_444();
        let ten_bit = bit_depth >= 10;
        let pq = hdr && ten_bit;
        if !chroma444 && (width % 2 != 0 || height % 2 != 0) {
            bail!("pyrowave 4:2:0 needs even dimensions (got {width}x{height})");
        }
        // Against the chroma actually being opened, not hardcoded 4:4:4. A 4:4:4 →
        // 4:2:0 downgrade hands oversized modes here as 4:2:0.
        if !crate::pyrowave_mode_fits_rdo(width, height, chroma444) {
            bail!(
                "pyrowave {} at {width}x{height} exceeds the rate controller's 16-bit block \
                 index (see pyrowave-sys patches/0002 note) — lower the resolution",
                if chroma444 { "4:4:4" } else { "4:2:0" }
            );
        }
        let fps = fps.max(1);
        // Vendor/device-id of the selected render adapter, not LUID: Session 0
        // Vulkan ICDs report `deviceLUIDValid = false`, so a LUID match finds nothing.
        let (vid, pid) = (vendor_id, device_id);
        // SAFETY: `create_device_by_compat` builds pyrowave's instance/device from
        // vendor/device-id (null uuids/luid = unconstrained); out-param is a live
        // local. Later calls take that non-null device; failure destroys it first.
        // Pointers are owned by the returned struct or freed on the error path.
        unsafe {
            let mut pw_dev: pw::pyrowave_device = std::ptr::null_mut();
            pw_check(
                pw::pyrowave_create_device_by_compat(
                    vid,
                    pid,
                    std::ptr::null(),
                    std::ptr::null(),
                    std::ptr::null(),
                    &mut pw_dev,
                ),
                "create_device_by_compat",
            )
            .with_context(|| {
                format!(
                    "open a PyroWave Vulkan device for GPU {vid:04x}:{pid:04x} (render adapter)"
                )
            })?;

            // Fail here (HEVC renegotiation) if this device cannot import external
            // memory, not at the first frame's import.
            if !pw::pyrowave_device_confirm_interop_support(pw_dev) {
                pw::pyrowave_device_destroy(pw_dev);
                bail!(
                    "the PyroWave Vulkan device does not confirm external-memory interop support \
                     (D3D11→Vulkan zero-copy import unavailable on this GPU / in this session \
                     context) — the session should renegotiate to HEVC"
                );
            }

            let einfo = pw::pyrowave_encoder_create_info {
                device: pw_dev,
                width: width as i32,
                height: height as i32,
                chroma: if chroma444 {
                    pw::pyrowave_chroma_subsampling_PYROWAVE_CHROMA_SUBSAMPLING_444
                } else {
                    pw::pyrowave_chroma_subsampling_PYROWAVE_CHROMA_SUBSAMPLING_420
                },
            };
            let mut pw_enc: pw::pyrowave_encoder = std::ptr::null_mut();
            if let Err(e) = pw_check(
                pw::pyrowave_encoder_create(&einfo, &mut pw_enc),
                "encoder_create",
            ) {
                pw::pyrowave_device_destroy(pw_dev);
                return Err(e);
            }

            let budget = FrameBudget::new(bitrate_bps, fps);
            tracing::info!(
                gpu = format!("{vid:04x}:{pid:04x}"),
                mode = %format!("{width}x{height}@{fps}"),
                budget_kib = budget.bytes / 1024,
                chroma = if chroma444 { "4:4:4" } else { "4:2:0" },
                ten_bit,
                hdr = pq,
                "PyroWave encoder open (Windows separate-plane zero-copy, intra-only wavelet)"
            );

            Ok(Self {
                pw_dev,
                pw_enc,
                sync: std::ptr::null_mut(),
                ring_gen: None,
                y_images: Vec::new(),
                cbcr_images: Vec::new(),
                width,
                height,
                chroma444,
                ten_bit,
                pq,
                budget,
                stream: AuStream::default(),
                bitstream: Vec::new(),
                pending: VecDeque::new(),
            })
        }
    }

    /// Import one capturer plane (`R8`/`R16` Y or `R8G8`/`R16G16` CbCr) into
    /// pyrowave's Vulkan device. Creates a fresh shared NT handle (`SHARED |
    /// SHARED_NTHANDLE`); `pyrowave_image_create` takes ownership and closes it
    /// only on success, so this fn closes on every failure return.
    ///
    /// # Safety
    /// `texture` is a live shareable `ID3D11Texture2D` of `vk_format`, size `w`×`h`,
    /// on `pw_dev`'s GPU. The returned image is owned by the caller. `pw_dev` is
    /// by value so cache closures do not double-borrow the encoder.
    unsafe fn import_plane(
        pw_dev: pw::pyrowave_device,
        texture: &ID3D11Texture2D,
        vk_format: pw::VkFormat,
        w: u32,
        h: u32,
    ) -> Result<pw::pyrowave_image> {
        let res: IDXGIResource1 = texture
            .cast()
            .context("ID3D11Texture2D -> IDXGIResource1 (plane not created shareable?)")?;
        // GENERIC_ALL (0x1000_0000): access the interop helper hands the shared handle.
        let handle: HANDLE = res
            .CreateSharedHandle(None, 0x1000_0000, PCWSTR::null())
            .context("IDXGIResource1::CreateSharedHandle(plane texture)")?;

        // Zeroed so pNext/queue-family/initialLayout stay 0 (null / UNDEFINED);
        // bindgen `Default` for the raw-pointer fields is not reliable.
        let mut ici: pw::VkImageCreateInfo = std::mem::zeroed();
        ici.sType = pw::VkStructureType_VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO;
        ici.imageType = pw::VkImageType_VK_IMAGE_TYPE_2D;
        ici.format = vk_format;
        ici.extent = pw::VkExtent3D {
            width: w,
            height: h,
            depth: 1,
        };
        ici.mipLevels = 1;
        ici.arrayLayers = 1;
        ici.samples = pw::VkSampleCountFlagBits_VK_SAMPLE_COUNT_1_BIT;
        ici.tiling = pw::VkImageTiling_VK_IMAGE_TILING_OPTIMAL;
        ici.usage = VK_IMAGE_USAGE_SAMPLED_BIT
            | VK_IMAGE_USAGE_TRANSFER_SRC_BIT
            | VK_IMAGE_USAGE_TRANSFER_DST_BIT;
        ici.sharingMode = pw::VkSharingMode_VK_SHARING_MODE_EXCLUSIVE;
        let info = pw::pyrowave_image_create_info {
            device: pw_dev,
            external_handle: handle.0 as usize as pw::pyrowave_os_handle,
            handle_type:
                pw::VkExternalMemoryHandleTypeFlagBits_VK_EXTERNAL_MEMORY_HANDLE_TYPE_D3D11_TEXTURE_BIT,
            image_create_info: &ici,
        };
        let mut image: pw::pyrowave_image = std::ptr::null_mut();
        if let Err(e) = pw_check(pw::pyrowave_image_create(&info, &mut image), "image_create") {
            // pyrowave consumes the handle only on success; on failure it is still
            // ours and this is the single close.
            let _ = CloseHandle(handle);
            return Err(e);
        }
        Ok(image)
    }

    /// Cache a plane import by `(texture address, width, height)`. Evict oldest
    /// past [`IMPORT_CACHE_CAP`]. A COM pointer here carries no reference, so a
    /// recycled address at a different size must not alias; same-size recycle is
    /// the ring-generation flush in `encode_frame`.
    ///
    /// # Safety
    /// Same contract as [`import_plane`].
    unsafe fn cached_plane(
        cache: &mut Vec<(PlaneKey, pw::pyrowave_image)>,
        make: impl FnOnce() -> Result<pw::pyrowave_image>,
        key: PlaneKey,
    ) -> Result<pw::pyrowave_image> {
        if let Some((_, img)) = cache.iter().find(|(k, _)| *k == key) {
            return Ok(*img);
        }
        let img = make()?;
        if cache.len() >= IMPORT_CACHE_CAP {
            let (_, old) = cache.remove(0);
            pw::pyrowave_image_destroy(old);
        }
        cache.push((key, img));
        Ok(img)
    }

    /// Import the capturer's shared fence as a Vulkan timeline. Pyrowave takes
    /// ownership and closes the handle, so this duplicates the capturer's
    /// persistent handle (needed for a later rebuild's re-import).
    ///
    /// # Safety
    /// `handle` is the capturer's live shared D3D11/D3D12 fence NT handle on
    /// `self.pw_dev`'s GPU.
    unsafe fn import_fence(&mut self, handle: isize) -> Result<()> {
        let mut dup = HANDLE::default();
        DuplicateHandle(
            GetCurrentProcess(),
            HANDLE(handle as *mut core::ffi::c_void),
            GetCurrentProcess(),
            &mut dup,
            0,
            false,
            DUPLICATE_SAME_ACCESS,
        )
        .context("DuplicateHandle(shared fence for pyrowave import)")?;
        let info = pw::pyrowave_sync_object_create_info {
            device: self.pw_dev,
            external_handle: dup.0 as usize as pw::pyrowave_os_handle,
            // D3D11 fence == D3D12 fence on Windows 10+; import as TIMELINE.
            handle_type:
                pw::VkExternalSemaphoreHandleTypeFlagBits_VK_EXTERNAL_SEMAPHORE_HANDLE_TYPE_D3D12_FENCE_BIT,
            semaphore_type: pw::VkSemaphoreType_VK_SEMAPHORE_TYPE_TIMELINE,
            import_flags: 0,
        };
        let mut sync: pw::pyrowave_sync_object = std::ptr::null_mut();
        if let Err(e) = pw_check(
            pw::pyrowave_sync_object_create(&info, &mut sync),
            "sync_object_create",
        ) {
            // pyrowave closes the handle only on success; close the dup on failure.
            let _ = CloseHandle(dup);
            return Err(e);
        }
        self.sync = sync;
        Ok(())
    }

    /// One synchronous frame: cache-import planes + fence, encode, packetize.
    ///
    /// # Safety
    /// Encode thread only; every pyrowave call takes handles this struct owns.
    unsafe fn encode_frame(&mut self, frame: &CapturedFrame) -> Result<()> {
        anyhow::ensure!(
            !self.pw_enc.is_null(),
            "pyrowave: encode after a failed reset (encoder was destroyed and not rebuilt)"
        );
        // Planes are imported at the encoder's configured extent. A capturer ring
        // recreate at a new mode would be read under a stale `VkImageCreateInfo`;
        // refuse — the session must reopen the encoder.
        anyhow::ensure!(
            frame.width == self.width && frame.height == self.height,
            "pyrowave: captured frame {}x{} != encoder {}x{} (the capturer recreated its ring at a \
             new mode — the encoder must be reopened)",
            frame.width,
            frame.height,
            self.width,
            self.height
        );
        let FramePayload::D3d11(d3d) = &frame.payload else {
            bail!("pyrowave (Windows) needs a D3D11 frame (the capturer must be in pyrowave mode)")
        };
        let share = d3d.pyro.as_ref().context(
            "pyrowave (Windows): the frame carries no PyroWave payload — the capturer was not opened \
             in pyrowave mode (session_plan::output_format must set OutputFormat::pyrowave)",
        )?;

        // Ring recreate: cached imports belong to freed textures whose COM
        // addresses can be reused. Flush on generation, not pointer identity.
        if self.ring_gen != Some(share.ring_gen) {
            if self.ring_gen.is_some() {
                tracing::info!(
                    from = ?self.ring_gen,
                    to = share.ring_gen,
                    cached = self.y_images.len() + self.cbcr_images.len(),
                    "pyrowave: capturer recreated its ring — flushing stale plane imports"
                );
            }
            for (_, img) in self.y_images.drain(..).chain(self.cbcr_images.drain(..)) {
                pw::pyrowave_image_destroy(img);
            }
            self.ring_gen = Some(share.ring_gen);
        }

        // First frame, or a rebuilt encoder: the capturer repeats the persistent
        // handle every frame so a fresh encoder can re-import it.
        if self.sync.is_null() {
            let h = share
                .fence_handle
                .context("pyrowave (Windows): frame carried no shared fence handle")?;
            self.import_fence(h)?;
        }

        // `pw_dev` is Copy so the cache closures do not borrow `self` alongside
        // `&mut self.*_images`.
        let (w, h) = (self.width, self.height);
        let (cw, ch) = if self.chroma444 {
            (w, h)
        } else {
            (w / 2, h / 2)
        };
        let (yf, cf) = if self.ten_bit {
            (
                pw::VkFormat_VK_FORMAT_R16_UNORM,
                pw::VkFormat_VK_FORMAT_R16G16_UNORM,
            )
        } else {
            (
                pw::VkFormat_VK_FORMAT_R8_UNORM,
                pw::VkFormat_VK_FORMAT_R8G8_UNORM,
            )
        };
        let pw_dev = self.pw_dev;
        let y_img = {
            let key = (d3d.texture.as_raw() as isize, w, h);
            let tex = &d3d.texture;
            Self::cached_plane(
                &mut self.y_images,
                || Self::import_plane(pw_dev, tex, yf, w, h),
                key,
            )?
        };
        let cbcr_img = {
            let key = (share.cbcr.as_raw() as isize, cw, ch);
            let tex = &share.cbcr;
            Self::cached_plane(
                &mut self.cbcr_images,
                || Self::import_plane(pw_dev, tex, cf, cw, ch),
                key,
            )?
        };

        // Y IDENTITY; Cb/Cr from the interleaved CbCr image via R/G swizzle
        // (same hand-off as Linux). GENERAL layout: pyrowave accepts it as-is.
        let y_vk = pw::pyrowave_image_get_handle(y_img);
        let cbcr_vk = pw::pyrowave_image_get_handle(cbcr_img);
        let plane = |image, pw_w, pw_h, fmt, swizzle| pw::pyrowave_image_view {
            image,
            width: pw_w,
            height: pw_h,
            image_format: fmt,
            view_format: fmt,
            mip_level: 0,
            layer: 0,
            aspect: pw::VkImageAspectFlagBits_VK_IMAGE_ASPECT_COLOR_BIT,
            swizzle,
            layout: pw::VkImageLayout_VK_IMAGE_LAYOUT_GENERAL,
        };
        let buffers = pw::pyrowave_gpu_buffers {
            planes: [
                plane(
                    y_vk,
                    w,
                    h,
                    yf,
                    pw::VkComponentSwizzle_VK_COMPONENT_SWIZZLE_IDENTITY,
                ),
                plane(
                    cbcr_vk,
                    cw,
                    ch,
                    cf,
                    pw::VkComponentSwizzle_VK_COMPONENT_SWIZZLE_R,
                ),
                plane(
                    cbcr_vk,
                    cw,
                    ch,
                    cf,
                    pw::VkComponentSwizzle_VK_COMPONENT_SWIZZLE_G,
                ),
            ],
        };

        // Acquire waits the capturer fence so the wavelet read is after CSC;
        // release returns the images to D3D11. Pyrowave owns the submission.
        let refs = [
            pw::pyrowave_gpu_external_reference {
                image: y_img,
                queue_family_index: VK_QUEUE_FAMILY_EXTERNAL,
            },
            pw::pyrowave_gpu_external_reference {
                image: cbcr_img,
                queue_family_index: VK_QUEUE_FAMILY_EXTERNAL,
            },
        ];
        let acquire = pw::pyrowave_gpu_sync_operation {
            images: refs.as_ptr(),
            num_images: refs.len(),
            sync: pw::pyrowave_sync_point {
                semaphore: pw::pyrowave_sync_object_get_semaphore(self.sync),
                value: share.fence_value,
            },
        };
        let release = pw::pyrowave_gpu_sync_operation {
            images: refs.as_ptr(),
            num_images: refs.len(),
            // Null semaphore: encode is synchronous and out-ring depth keeps the
            // slot unused until the next encode completes (same as NVENC).
            sync: std::mem::zeroed(),
        };
        let rc = pw::pyrowave_rate_control {
            maximum_bitstream_size: self.budget.rate_control(self.stream.wire_chunk.is_some()),
        };
        pw_check(
            pw::pyrowave_encoder_encode_gpu_synchronous(
                self.pw_enc,
                &acquire,
                &release,
                &buffers,
                &rc,
            ),
            "encode_gpu_synchronous",
        )?;

        let wire_chunk = self.stream.wire_chunk;
        let pkts = packetize(
            self.pw_enc,
            &mut self.bitstream,
            self.budget.bytes + BS_SLACK,
            wire_chunk,
            self.pq,
        )?;
        let au = pyrowave_wire::build_au(&pkts, &self.bitstream, wire_chunk);
        if wire_chunk.is_some() {
            self.budget.observe(&pkts, au.len());
        }
        self.pending.push_back(EncodedFrame {
            data: au,
            pts_ns: frame.pts_ns,
            keyframe: true,
            recovery_anchor: false,
            recovery_point: false,
            recovery_close: false,
            chunk_aligned: wire_chunk.is_some(),
        });
        Ok(())
    }
}

impl Encoder for PyroWaveEncoder {
    fn submit(&mut self, frame: &CapturedFrame) -> Result<()> {
        // SAFETY: encode thread only; `encode_frame` uses handles this struct owns
        // and pyrowave waits its own fence before packetize returns.
        unsafe { self.encode_frame(frame) }
    }

    fn caps(&self) -> EncoderCaps {
        // No RFI (every frame is intra). Report the opened chroma: a hardcoded
        // `default()` would report a 4:4:4 session as 4:2:0.
        EncoderCaps {
            // The Windows capturer composites the pointer; this backend never reads it.
            blends_cursor: false,
            chroma_444: self.chroma444,
            ..EncoderCaps::default()
        }
    }

    fn poll(&mut self) -> Result<Option<EncodedFrame>> {
        self.stream.check_whole_poll()?;
        Ok(self.pending.pop_front())
    }

    // Cutting lives in [`AuStream`] (compiles on every platform). This file cannot be built
    // off Windows, so the helper is the verified path.
    fn supports_chunked_poll(&self) -> bool {
        self.stream.supports_chunked_poll()
    }

    fn poll_chunk(&mut self) -> Result<Option<crate::AuChunk>> {
        if let Some(chunk) = self.stream.next_open() {
            return Ok(Some(chunk));
        }
        // No wait: `submit` already encoded synchronously, so `pending` is complete.
        Ok(self.pending.pop_front().and_then(|f| self.stream.cut(f)))
    }

    fn reset(&mut self) -> bool {
        // Drop the cursor before `pending.clear()` so the next `poll_chunk` cannot
        // splice a dead AU's tail onto a fresh one.
        self.stream.reset();
        // Recreate only the encoder object; device, imported textures and fence survive.
        // SAFETY: encode is synchronous (no work in flight); the device outlives the swapped encoder.
        unsafe {
            pw::pyrowave_encoder_destroy(self.pw_enc);
            // Null immediately: create below is fallible and destroy is not
            // null-safe, so a leftover freed pointer is a `Drop` double-free.
            self.pw_enc = std::ptr::null_mut();
            let einfo = pw::pyrowave_encoder_create_info {
                device: self.pw_dev,
                width: self.width as i32,
                height: self.height as i32,
                // Session chroma, not hardcoded 420: a 4:4:4 CSC must not feed a 4:2:0 encoder.
                chroma: if self.chroma444 {
                    pw::pyrowave_chroma_subsampling_PYROWAVE_CHROMA_SUBSAMPLING_444
                } else {
                    pw::pyrowave_chroma_subsampling_PYROWAVE_CHROMA_SUBSAMPLING_420
                },
            };
            let mut enc: pw::pyrowave_encoder = std::ptr::null_mut();
            let r = pw::pyrowave_encoder_create(&einfo, &mut enc);
            if r != pw::pyrowave_result_PYROWAVE_SUCCESS {
                tracing::error!(result = ?r, "pyrowave: encoder rebuild failed");
                // `pw_enc` stays null; `Drop` and `encode_frame` both guard on it.
                self.pending.clear();
                return false;
            }
            self.pw_enc = enc;
        }
        self.pending.clear();
        true
    }

    fn reconfigure_bitrate(&mut self, bps: u64) -> bool {
        self.budget.retarget(bps);
        true
    }

    fn set_wire_chunking(&mut self, shard_payload: usize) {
        if self.stream.set_chunking(shard_payload) {
            tracing::info!(
                shard_payload,
                "pyrowave: datagram-aligned packetization on (partial-frame loss mode)"
            );
        }
    }

    fn flush(&mut self) -> Result<()> {
        Ok(())
    }
}

impl Drop for PyroWaveEncoder {
    fn drop(&mut self) {
        // SAFETY: owned handles, destroyed once; encoder/images/sync go before
        // the device they borrow (pyrowave.h).
        unsafe {
            // Null after a failed `reset()`; `pyrowave_encoder_destroy` is not null-safe.
            if !self.pw_enc.is_null() {
                pw::pyrowave_encoder_destroy(self.pw_enc);
            }
            for (_, img) in self.y_images.drain(..).chain(self.cbcr_images.drain(..)) {
                pw::pyrowave_image_destroy(img);
            }
            if !self.sync.is_null() {
                pw::pyrowave_sync_object_destroy(self.sync);
            }
            pw::pyrowave_device_destroy(self.pw_dev);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pyrowave_ffi::oracle;
    use pf_frame::dxgi::{D3d11Frame, PyroFrameShare};
    use pf_frame::PixelFormat;
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_1};
    use windows::Win32::Graphics::Direct3D11::{
        D3D11CreateDevice, ID3D11Device, ID3D11Device5, ID3D11DeviceContext, ID3D11DeviceContext4,
        ID3D11Fence, ID3D11Texture2D, D3D11_BIND_RENDER_TARGET, D3D11_CPU_ACCESS_WRITE,
        D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_FENCE_FLAG_SHARED, D3D11_MAPPED_SUBRESOURCE,
        D3D11_MAP_WRITE, D3D11_RESOURCE_MISC_SHARED, D3D11_RESOURCE_MISC_SHARED_NTHANDLE,
        D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING,
    };
    use windows::Win32::Graphics::Dxgi::Common::{
        DXGI_FORMAT, DXGI_FORMAT_R16G16_UNORM, DXGI_FORMAT_R16_UNORM, DXGI_FORMAT_R8G8_UNORM,
        DXGI_FORMAT_R8_UNORM, DXGI_SAMPLE_DESC,
    };

    /// Shareable plane texture (`bpp` bytes/texel) filled with `bytes` via staging.
    /// Same SHARED|SHARED_NTHANDLE + RENDER_TARGET flags as the capturer out-ring.
    ///
    /// # Safety
    /// `bytes.len() == bpp`; `device`/`context` are live.
    unsafe fn make_plane(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        w: u32,
        h: u32,
        format: DXGI_FORMAT,
        bpp: usize,
        bytes: &[u8],
    ) -> ID3D11Texture2D {
        let mut desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: (D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0 | D3D11_RESOURCE_MISC_SHARED.0)
                as u32,
        };
        let mut tex: Option<ID3D11Texture2D> = None;
        device
            .CreateTexture2D(&desc, None, Some(&mut tex))
            .expect("CreateTexture2D(plane default)");
        let tex = tex.unwrap();
        desc.BindFlags = 0;
        desc.MiscFlags = 0;
        desc.Usage = D3D11_USAGE_STAGING;
        desc.CPUAccessFlags = D3D11_CPU_ACCESS_WRITE.0 as u32;
        let mut staging: Option<ID3D11Texture2D> = None;
        device
            .CreateTexture2D(&desc, None, Some(&mut staging))
            .expect("CreateTexture2D(plane staging)");
        let staging = staging.unwrap();
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        context
            .Map(&staging, 0, D3D11_MAP_WRITE, 0, Some(&mut mapped))
            .expect("Map(plane staging)");
        let pitch = mapped.RowPitch as usize;
        let base = mapped.pData as *mut u8;
        for row in 0..(h as usize) {
            let r = base.add(row * pitch);
            for x in 0..(w as usize) {
                for (b, &v) in bytes.iter().enumerate() {
                    *r.add(x * bpp + b) = v;
                }
            }
        }
        context.Unmap(&staging, 0);
        context.CopyResource(&tex, &staging);
        tex
    }

    /// Zero-copy smoke: solid Y≠Cb≠Cr in separate shareable planes → encode →
    /// decode. Distinct fills so a plane swap cannot hide. Returns plane means.
    ///
    /// # Safety
    /// Real D3D11 + Vulkan 1.3 GPU; all COM/FFI handles are locally owned.
    unsafe fn run_case(w: u32, h: u32, hdr: bool, chroma444: bool) -> (f64, f64, f64) {
        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_1]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
        .expect("D3D11CreateDevice");
        let device = device.unwrap();
        let context = context.unwrap();

        // 16-bit fills are v8 * 257 (0xVV,0xVV LE); UNORM equals v8/255 exactly,
        // so 8-bit decode means stay 100/180/60 in every mode.
        let (cw, ch) = if chroma444 { (w, h) } else { (w / 2, h / 2) };
        let (y_tex, cbcr_tex) = if hdr {
            (
                make_plane(
                    &device,
                    &context,
                    w,
                    h,
                    DXGI_FORMAT_R16_UNORM,
                    2,
                    &[0x64, 0x64],
                ),
                make_plane(
                    &device,
                    &context,
                    cw,
                    ch,
                    DXGI_FORMAT_R16G16_UNORM,
                    4,
                    &[0xB4, 0xB4, 0x3C, 0x3C],
                ),
            )
        } else {
            (
                make_plane(&device, &context, w, h, DXGI_FORMAT_R8_UNORM, 1, &[100]),
                make_plane(
                    &device,
                    &context,
                    cw,
                    ch,
                    DXGI_FORMAT_R8G8_UNORM,
                    2,
                    &[180, 60],
                ),
            )
        };

        // Signalled after the fills (capturer convert→signal order).
        let dev5: ID3D11Device5 = device.cast().expect("ID3D11Device5");
        let mut fence: Option<ID3D11Fence> = None;
        dev5.CreateFence(0, D3D11_FENCE_FLAG_SHARED, &mut fence)
            .expect("CreateFence");
        let fence = fence.unwrap();
        let fence_handle = fence
            .CreateSharedHandle(None, 0x1000_0000, windows::core::PCWSTR::null())
            .expect("fence CreateSharedHandle");
        let ctx4: ID3D11DeviceContext4 = context.cast().expect("ID3D11DeviceContext4");
        ctx4.Signal(&fence, 1).expect("Signal");
        context.Flush();

        let mut enc = PyroWaveEncoder::open(
            w,
            h,
            60,
            100_000_000,
            if chroma444 {
                crate::ChromaFormat::Yuv444
            } else {
                crate::ChromaFormat::Yuv420
            },
            if hdr { 10 } else { 8 },
            hdr,
            0,
            0,
        )
        .expect("PyroWaveEncoder::open");
        let frame = CapturedFrame {
            provenance: Default::default(),
            width: w,
            height: h,
            pts_ns: 0,
            format: PixelFormat::Nv12,
            payload: FramePayload::D3d11(D3d11Frame {
                texture: y_tex,
                device: device.clone(),
                pyro: Some(PyroFrameShare {
                    cbcr: cbcr_tex,
                    fence_handle: Some(fence_handle.0 as isize),
                    fence_value: 1,
                    // Constant generation: the cache-hit path. A changing one would flush every frame.
                    ring_gen: 1,
                }),
            }),
            cursor: None,
        };
        enc.submit(&frame).expect("submit");
        let au = enc.poll().expect("poll").expect("one AU per frame");
        assert!(au.keyframe, "every pyrowave AU is a keyframe");
        assert!(!au.data.is_empty(), "AU is non-empty");
        // Dense AU starts with the 8-byte sequence header; LIMITED is bit 30
        // (byte 7 bit 6 = 0x40). Pyrowave zeros this as FULL.
        assert_eq!(
            au.data[7] & 0x40,
            0x40,
            "sequence header must signal ycbcr_range=LIMITED"
        );
        if hdr {
            assert_eq!(
                au.data[7] & 0x78,
                0x78,
                "HDR sequence header must signal BT.2020 primaries + PQ + BT.2020 matrix"
            );
        }
        let planes = oracle::decode_planes(w, h, &[&au.data], chroma444);
        oracle::plane_means(&planes[0])
    }

    /// End-to-end on a real GPU. `#[ignore]`d; build anywhere, run on the GPU host:
    /// `cargo test -p pf-encode --features pyrowave --no-run`
    /// then `<bin> --ignored --nocapture pyrowave_win_smoke`.
    /// Square plus real streaming sizes: NVIDIA D3D11→Vulkan import is size-sensitive.
    #[test]
    #[ignore = "needs a real D3D11 + Vulkan-1.3 GPU (run on the Windows host, not the build box)"]
    fn pyrowave_win_smoke() {
        // SDR 4:2:0 across streaming sizes (NVIDIA import size check), then the
        // other (hdr, chroma) modes at two sizes — R16 and full-res-chroma imports.
        let mut cases = vec![
            (1024u32, 1024u32, false, false),
            (1280, 720, false, false),
            (1920, 1080, false, false),
            (2560, 1440, false, false),
        ];
        for &(hdr, c444) in &[(false, true), (true, false), (true, true)] {
            cases.push((1280, 720, hdr, c444));
            cases.push((1920, 1080, hdr, c444));
        }
        for (w, h, hdr, c444) in cases {
            // SAFETY: single-threaded test; `run_case` owns every COM/FFI handle it touches.
            let (ym, cbm, crm) = unsafe { run_case(w, h, hdr, c444) };
            eprintln!(
                "{w}x{h} hdr={hdr} 444={c444}: decoded means Y={ym:.1} Cb={cbm:.1} Cr={crm:.1} \
                 (expect 100/180/60)"
            );
            assert!(
                (ym - 100.0).abs() < 6.0 && (cbm - 180.0).abs() < 6.0 && (crm - 60.0).abs() < 6.0,
                "{w}x{h} hdr={hdr} 444={c444}: round-trip means (Y {ym:.1}, Cb {cbm:.1}, \
                 Cr {crm:.1}) drifted from the filled 100/180/60 — plane mapping/format wrong"
            );
        }
    }
}
