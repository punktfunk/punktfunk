//! The per-frame record of the Vulkan Video encoder, in stages with one job each:
//! `plan_coding` (the frame's references), `acquire_source` (the native import, an RGB-direct
//! arm, or the CSC's input in an open compute batch), `record_csc`, `record_coding` (HEVC or
//! AV1), `submit`, then the DPB bookkeeping. `enqueue` in the parent calls `record_submit` once
//! per frame with a free ring slot.
//!
//! A failed stage returns its error to `enqueue`, which hands the slot back with the frame's
//! armed loss and forgets its cursor upload (the ring's `Return` and `ForgetCursor`). Only the
//! CSC arm records a fallible step into a begun batch, so only it resets one on error.
//!
//! A `#[path]` child of `vulkan_video.rs`, so it sees the parent's private items (`Frame`,
//! `OpenSpec`, the encoder's fields) without widening them. Design:
//! `design/pf-encode-overhaul.md` §4.3.

// `unsafe_op_in_unsafe_fn` is off here, as in the parent: every body is raw ash calls.
#![allow(unsafe_op_in_unsafe_fn)]

use super::*;
use crate::cpu_ingest::Target;

/// The frame's references, planned before anything records.
struct CodingPlan {
    frame: FramePlan,
    is_idr: bool,
    /// An error-resilient anchor. A confirmed reference is anchor-tagged only: the client
    /// decoded it and holds its CDFs.
    recovery: bool,
    ref_slot: usize,
    setup_idx: usize,
    poc: i32,
}

/// What `acquire_source` leaves for the next stage.
enum SourceAcquire {
    /// The slot's compute batch is recording with the RGB source in it; the CSC comes next.
    Csc(CscSource),
    /// The encode reads this source; there is no CSC.
    Encode(EncodeSource),
}

/// The CSC's input in the slot's open compute batch.
struct CscSource {
    rgb_view: vk::ImageView,
    /// The imported dmabuf the CSC samples, handed back to its producer after the dispatch.
    imported: Option<vk::Image>,
    /// That dmabuf as words, where the device needs a LINEAR source read that way.
    lin: Option<LinearSrc>,
    /// The cursor blend as `[origin_x, origin_y, size_w, size_h]`; size 0 skips it.
    cursor_pc: [i32; 4],
}

/// The picture the encode reads and how its batch acquires it.
struct EncodeSource {
    img: vk::Image,
    view: vk::ImageView,
    acquire: SrcAcquire,
    /// The slot's compute batch writes the source, so the encode waits on `csc_sem`.
    compute: bool,
}

impl VulkanVideoEncoder {
    /// Record `frame` into ring `slot` and submit it without waiting; `read_slot` reads the AU
    /// once the slot's fence signals. `slot` must be free (its last AU already read).
    pub(super) unsafe fn record_submit(
        &mut self,
        slot: usize,
        frame: &CapturedFrame,
        wire: i64,
    ) -> Result<()> {
        let plan = self.plan_coding();
        let src = match self.acquire_source(slot, frame)? {
            SourceAcquire::Csc(csc) => self.record_csc(slot, &csc)?,
            SourceAcquire::Encode(src) => src,
        };
        self.record_coding(slot, &src, &plan)?;
        self.submit(slot, src.compute)?;
        self.post_submit_bookkeeping(
            slot,
            frame.pts_ns,
            wire,
            plan.frame,
            plan.setup_idx,
            plan.poc,
        );
        Ok(())
    }

    /// Plan the frame's references against the slot table as it stands. Takes the armed loss
    /// and sets this frame's wave; bookkeeping advances the wave once the frame is queued.
    fn plan_coding(&mut self) -> CodingPlan {
        // A pending rate stays pending through recording: begin-coding declares the rate the
        // session has installed, and bookkeeping promotes the new one after (VUID-08254).

        // An armed loss resolves here: the taint sweep ran at the report. The armed loss and
        // the wave are restored by the caller (`Return`) when the record fails, so the retry
        // plans the same frame. A confirmed reference is looked up only when no loss is armed.
        let armed = self.pending_loss.take();
        let floor = self.reference_floor;
        let refs = match (armed, floor) {
            (None, None) => Vec::new(),
            _ => trusted_refs(&self.slot_wire),
        };
        let anchor = armed.and_then(|lf| crate::rfi::pick_recovery(&refs, lf, floor.as_ref()));
        let confirmed = floor
            .filter(|_| armed.is_none())
            .and_then(|c| crate::rfi::pick_acked(&refs, &c));
        let pending = Pending {
            idr: self.first_frame || self.force_kf,
            anchor: anchor.is_some(),
            acked: confirmed.is_some(),
            loss: armed.is_some(),
        };
        let state = Recovery {
            wave: self.wave,
            ..Recovery::default()
        };
        let plan = plan_frame(state, pending, &self.recovery_policy());
        let is_idr = plan.refs == FrameRef::Idr;
        let ref_slot = match (plan.refs, anchor, confirmed) {
            (FrameRef::Anchor, Some((s, _)), _) | (FrameRef::Acked, _, Some((s, _))) => s,
            _ => self.prev_slot,
        };
        if let Some(loss_first) = armed {
            tracing::debug!(
                loss_first,
                refs = ?plan.refs,
                wave = ?plan.wave,
                ref_slot,
                "vulkan-encode: armed loss resolved at frame-build"
            );
        }
        // The record paths read this frame's wave; bookkeeping advances it.
        self.wave = plan.wave;
        let poc: i32 = if is_idr { 0 } else { self.poc };
        // An IDR empties the DPB, kept pictures included.
        let setup_idx = if is_idr {
            (self.enc_count % DPB_SLOTS as u64) as usize
        } else {
            setup_slot(self.enc_count, ref_slot, &self.slot_wire, &self.kept)
        };
        CodingPlan {
            frame: plan,
            is_idr,
            recovery: plan.refs == FrameRef::Anchor,
            ref_slot,
            setup_idx,
            poc,
        }
    }

    /// The encode source for the session's mode: the native planar import, the CSC's input,
    /// or an RGB-direct arm. EFC converts inline and cannot composite (`open` refuses RGB-direct
    /// for a cursor-blend session); only its padded copy and CPU upload use the compute queue.
    unsafe fn acquire_source(
        &mut self,
        slot: usize,
        frame: &CapturedFrame,
    ) -> Result<SourceAcquire> {
        if self.spec.native_nv12 {
            return self.acquire_native(frame).map(SourceAcquire::Encode);
        }
        if self.rgb.is_none() {
            return self.acquire_csc(slot, frame).map(SourceAcquire::Csc);
        }
        let compute_cmd = self.frames[slot].compute_cmd;
        let ts_pool = self.frames[slot].ts_pool;
        let padded = self.rgb.as_ref().is_some_and(|r| r.padded);
        // Only the padded Dmabuf arm records timestamps (`record_pad_blit`); CPU-upload writes none.
        self.frames[slot].ts_written = false;
        let src = match &frame.payload {
            FramePayload::Dmabuf(d) if !padded => {
                // Imported buffer must cover the declared source extent (aligned coded extent,
                // or render size in true-extent mode). A mismatch takes the encoder-rebuild
                // path instead of letting EFC read past the allocation.
                let (need_w, need_h) = if self.rgb.as_ref().is_some_and(|r| r.true_extent) {
                    (self.spec.width, self.spec.height)
                } else {
                    (self.width, self.height)
                };
                if frame.width != need_w || frame.height != need_h {
                    bail!(
                        "vulkan-encode (rgb-direct): frame {}x{} does not cover the declared \
                         source extent {}x{} — refusing an out-of-bounds encode source",
                        frame.width,
                        frame.height,
                        need_w,
                        need_h
                    );
                }
                let (img, view, fresh) = self.import_cached(d, frame.width, frame.height)?;
                let acquire = if fresh {
                    SrcAcquire::DmabufFresh
                } else {
                    SrcAcquire::DmabufCached
                };
                EncodeSource {
                    img,
                    view,
                    acquire,
                    compute: false,
                }
            }
            FramePayload::Dmabuf(d) => {
                // Unaligned: blit into per-slot ALIGNED staging and edge-duplicate. Encode
                // reads staging, never the capture buffer.
                if frame.width != self.spec.width || frame.height != self.spec.height {
                    bail!(
                        "vulkan-encode (rgb-direct/padded): frame {}x{} != mode {}x{} — \
                         refusing a mismatched blit source",
                        frame.width,
                        frame.height,
                        self.spec.width,
                        self.spec.height
                    );
                }
                let (img, _view, fresh) = self.import_cached(d, frame.width, frame.height)?;
                let pad_img = self.frames[slot].pad.image;
                let pad_view = self.frames[slot].pad.view;
                self.record_pad_blit(
                    &self.device,
                    compute_cmd,
                    img,
                    fresh,
                    pad_img,
                    ts_pool,
                    &[(vk::ImageAspectFlags::COLOR, 1)],
                )?;
                self.frames[slot].ts_written = self.ts_period_ns > 0.0;
                // Staging ends in GENERAL; `csc_sem` orders the hand-off ([`SrcAcquire::CscGeneral`]).
                EncodeSource {
                    img: pad_img,
                    view: pad_view,
                    acquire: SrcAcquire::CscGeneral,
                    compute: true,
                }
            }
            FramePayload::Cpu(_) => {
                // `begin_command_buffer` is after the fallible staging, so no reset-on-error
                // wrap. `compute_cmd` is only the staging→image copy; encode waits on `csc_sem`.
                let view = self.stage_cpu(slot, frame)?;
                self.device.begin_command_buffer(
                    compute_cmd,
                    &vk::CommandBufferBeginInfo::default()
                        .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
                )?;
                let img = self.record_cpu_copy(slot, compute_cmd, (self.width, self.height));
                self.device.end_command_buffer(compute_cmd)?;
                EncodeSource {
                    img,
                    view,
                    acquire: SrcAcquire::CpuUpload,
                    compute: true,
                }
            }
            _ => bail!(
                "vulkan-encode (rgb-direct): unsupported FramePayload (need Dmabuf or Cpu RGB)"
            ),
        };
        Ok(SourceAcquire::Encode(src))
    }

    /// The native planar arm: the producer's visible-size NV12/P010 buffer is the encode source.
    /// Safe at every mode because native sessions run true-size headers (see
    /// [`OpenSpec::native_nv12`]). No compositing stage: the session plan negotiates native NV12
    /// only when `!cursor_blend`.
    unsafe fn acquire_native(&mut self, frame: &CapturedFrame) -> Result<EncodeSource> {
        let want = self.spec.native_fmt;
        if frame.format != want {
            bail!(
                "vulkan-encode (native planar): negotiated {want:?} but received {:?}",
                frame.format
            );
        }
        if frame.width != self.spec.width || frame.height != self.spec.height {
            bail!(
                "vulkan-encode (native planar): frame {}x{} != mode {}x{}",
                frame.width,
                frame.height,
                self.spec.width,
                self.spec.height
            );
        }
        if frame.width % 2 != 0 || frame.height % 2 != 0 {
            bail!("vulkan-encode (native planar): 4:2:0 frame dimensions must be even");
        }
        let FramePayload::Dmabuf(d) = &frame.payload else {
            bail!("vulkan-encode (native planar): producer frame is not a DMA-BUF");
        };
        if d.fourcc != pf_frame::drm_fourcc(want).expect("planar FourCC") {
            bail!(
                "vulkan-encode (native planar): DMA-BUF FourCC {:#x} does not match {want:?}",
                d.fourcc
            );
        }
        if d.modifier != 0 {
            bail!(
                "vulkan-encode (native planar): only LINEAR is supported, got modifier {:#x}",
                d.modifier
            );
        }
        let (img, view, fresh) = self.import_cached(d, frame.width, frame.height)?;
        let acquire = if fresh {
            SrcAcquire::DmabufFresh
        } else {
            SrcAcquire::DmabufCached
        };
        Ok(EncodeSource {
            img,
            view,
            acquire,
            compute: false,
        })
    }

    /// The CSC arm: begins the slot's compute batch with the timestamp, the cursor upload and
    /// the RGB source (a dmabuf acquired for the shader, or the CPU upload) for `record_csc`.
    unsafe fn acquire_csc(&mut self, slot: usize, frame: &CapturedFrame) -> Result<CscSource> {
        // Shader samples with clamped 1:1 texelFetch, so a mismatch silently streams a
        // cropped/edge-padded picture. Refuse into the encoder-rebuild path instead. A reframed
        // session needs only a capture that holds its crop.
        let fits = match &self.reframe {
            Some(r) => r.covers(frame.width, frame.height),
            None => frame.width == self.spec.width && frame.height == self.spec.height,
        };
        if !fits {
            bail!(
                "vulkan-encode (csc): frame {}x{} != mode {}x{} — refusing a mismatched CSC \
                 source",
                frame.width,
                frame.height,
                self.spec.width,
                self.spec.height
            );
        }
        let dev = self.device.clone(); // handle clone so `&mut self` helpers still work
        let compute_cmd = self.frames[slot].compute_cmd;
        let ts_pool = self.frames[slot].ts_pool;
        dev.begin_command_buffer(
            compute_cmd,
            &vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
        )?;
        // Fallible prefix in one closure whose error arm resets `compute_cmd`. Leaving it
        // RECORDING makes the next `begin` violate VUID-vkBeginCommandBuffer-commandBuffer-00049.
        // Never PENDING here: nothing is submitted until `submit`, so the reset is legal.
        let prefix = (|| -> Result<CscSource> {
            self.frames[slot].ts_written = false;
            if self.ts_period_ns > 0.0 {
                dev.cmd_reset_query_pool(compute_cmd, ts_pool, 0, 2);
                dev.cmd_write_timestamp2(compute_cmd, vk::PipelineStageFlags2::NONE, ts_pool, 0);
                self.frames[slot].ts_written = true;
            }

            let cursor_pc = self.frames[slot].cursor.prep(
                &dev,
                compute_cmd,
                frame.cursor.as_ref(),
                self.spec.is_hdr,
            )?;

            let (rgb_view, imported, lin) = match &frame.payload {
                FramePayload::Dmabuf(d) => {
                    let (img, view, fresh) = self.import_cached(d, frame.width, frame.height)?;
                    let lin = self.words.source(
                        &dev,
                        &self.ext_fd,
                        &self.mem_props,
                        self.import_key(d),
                        d,
                        (frame.width, frame.height),
                    )?;
                    // EXCLUSIVE sharing: acquire the image from the foreign producer family for
                    // this compute read, fresh (UNDEFINED preserves modifier-tiled bytes) or
                    // cached (GENERAL, where `record_csc`'s release left it). `Frame::src_hold`
                    // covers content stability, not queue ownership.
                    let acq = imported_acquire_barrier(
                        img,
                        fresh,
                        self.foreign_qfi,
                        self.compute_family,
                        vk::PipelineStageFlags2::COMPUTE_SHADER,
                        vk::AccessFlags2::SHADER_READ,
                        vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                    );
                    dev.cmd_pipeline_barrier2(
                        compute_cmd,
                        &vk::DependencyInfo::default().image_memory_barriers(&[acq]),
                    );
                    (view, Some(img), lin)
                }
                FramePayload::Cpu(_) => {
                    let view = self.stage_cpu(slot, frame)?;
                    // Staging is tightly packed and the image is source-sized; an aligned-size
                    // extent here shears rows.
                    let img = self.record_cpu_copy(slot, compute_cmd, (frame.width, frame.height));
                    let to_read = vk::ImageMemoryBarrier2::default()
                        .src_stage_mask(vk::PipelineStageFlags2::ALL_TRANSFER)
                        .src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
                        .dst_stage_mask(vk::PipelineStageFlags2::COMPUTE_SHADER)
                        .dst_access_mask(vk::AccessFlags2::SHADER_READ)
                        .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                        .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                        .image(img)
                        .subresource_range(color_range(0));
                    dev.cmd_pipeline_barrier2(
                        compute_cmd,
                        &vk::DependencyInfo::default().image_memory_barriers(&[to_read]),
                    );
                    (view, None, None)
                }
                _ => bail!("vulkan-encode: unsupported FramePayload (need Dmabuf or Cpu RGB)"),
            };
            Ok(CscSource {
                rgb_view,
                imported,
                lin,
                cursor_pc,
            })
        })();
        if prefix.is_err() {
            // RECORDING (never submitted yet); the pool allows the reset. A cursor upload the
            // reset discards is forgotten by the caller (`ForgetCursor`).
            let _ = dev.reset_command_buffer(compute_cmd, vk::CommandBufferResetFlags::empty());
        }
        prefix
    }

    /// Stage a CPU frame in the slot's upload buffer as 4-byte pixels ([`CpuIngest`];
    /// `ensure_cpu_rgb` pads with 4-bpp index math). Host-side only; returns the image's view.
    unsafe fn stage_cpu(&mut self, slot: usize, frame: &CapturedFrame) -> Result<vk::ImageView> {
        // The CPU channel order follows the session picture format: RGB-direct encodes the
        // upload as its `pictureFormat`, which followed the first frame
        // (VUID-vkCmdEncodeVideoKHR-pEncodeInfo-08207). The CSC samples either order.
        let target = if self.rgb.is_some() && self.spec.src_rgb_fmt == vk::Format::B8G8R8A8_UNORM {
            Target::BFirst
        } else {
            Target::AsCaptured
        };
        // Out while `ensure_cpu_rgb` borrows `self`.
        let mut ingest = std::mem::take(&mut self.cpu_ingest);
        let view = ingest.stage(frame, target).and_then(|staged| {
            let fmt = pixel_to_vk(staged.fmt).context("unsupported CPU pixel format")?;
            self.ensure_cpu_rgb(slot, fmt, staged.bytes, frame.width, frame.height)
        });
        self.cpu_ingest = ingest;
        view
    }

    /// Record the copy of the slot's staged CPU payload into its upload image, `width` ×
    /// `height`, into `compute_cmd`, which is recording. Returns the image, in TRANSFER_DST.
    unsafe fn record_cpu_copy(
        &self,
        slot: usize,
        compute_cmd: vk::CommandBuffer,
        (width, height): (u32, u32),
    ) -> vk::Image {
        let dev = &self.device;
        let img = self.frames[slot]
            .cpu_img
            .expect("ensure_cpu_rgb built it")
            .0
            .image;
        let stage = self.frames[slot]
            .cpu_stage
            .expect("ensure_cpu_rgb built it")
            .0
            .buffer;
        let to_dst = vk::ImageMemoryBarrier2::default()
            .src_stage_mask(vk::PipelineStageFlags2::NONE)
            .src_access_mask(vk::AccessFlags2::NONE)
            .dst_stage_mask(vk::PipelineStageFlags2::ALL_TRANSFER)
            .dst_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
            .old_layout(vk::ImageLayout::UNDEFINED)
            .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(img)
            .subresource_range(color_range(0));
        dev.cmd_pipeline_barrier2(
            compute_cmd,
            &vk::DependencyInfo::default().image_memory_barriers(&[to_dst]),
        );
        dev.cmd_copy_buffer_to_image(
            compute_cmd,
            stage,
            img,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            &[vk::BufferImageCopy::default()
                .image_subresource(
                    vk::ImageSubresourceLayers::default()
                        .aspect_mask(vk::ImageAspectFlags::COLOR)
                        .layer_count(1),
                )
                .image_extent(vk::Extent3D {
                    width,
                    height,
                    depth: 1,
                })],
        );
        img
    }

    /// Copy the visible frame into aligned staging plus edge-duplicate into 64×16 padding
    /// (same edge semantics as the CSC shader's clamped reads). Transfer-only. Staging lives
    /// in GENERAL (copy dst and, for the right-column pass, copy src); encode acquires via
    /// [`SrcAcquire::CscGeneral`].
    ///
    /// `planes`: `[(COLOR, 1)]` packed RGB, `[(PLANE_0, 1), (PLANE_1, 2)]` NV12. Multi-planar
    /// copy regions are in each plane's own coordinate space; barriers stay COLOR-aspect.
    /// Every divisor must divide visible and aligned extents.
    #[allow(clippy::too_many_arguments)]
    unsafe fn record_pad_blit(
        &self,
        dev: &ash::Device,
        compute_cmd: vk::CommandBuffer,
        src: vk::Image,
        src_fresh: bool,
        pad: vk::Image,
        ts_pool: vk::QueryPool,
        planes: &[(vk::ImageAspectFlags, u32)],
    ) -> Result<()> {
        let (rw, rh) = (self.spec.width, self.spec.height);
        let (w, h) = (self.width, self.height);
        dev.begin_command_buffer(
            compute_cmd,
            &vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
        )?;
        if self.ts_period_ns > 0.0 {
            dev.cmd_reset_query_pool(compute_cmd, ts_pool, 0, 2);
            dev.cmd_write_timestamp2(compute_cmd, vk::PipelineStageFlags2::NONE, ts_pool, 0);
        }
        // EXCLUSIVE sharing: acquire the import from the producer family for this blit
        // (UNDEFINED on a fresh import preserves modifier-tiled bytes; cached sits in
        // GENERAL where the release below left it). Staging: transfer-write, discarded.
        let src_acq = imported_acquire_barrier(
            src,
            src_fresh,
            self.foreign_qfi,
            self.compute_family,
            vk::PipelineStageFlags2::ALL_TRANSFER,
            vk::AccessFlags2::TRANSFER_READ,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
        );
        let pad_dst = vk::ImageMemoryBarrier2::default()
            .src_stage_mask(vk::PipelineStageFlags2::NONE)
            .src_access_mask(vk::AccessFlags2::NONE)
            .dst_stage_mask(vk::PipelineStageFlags2::ALL_TRANSFER)
            .dst_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
            .old_layout(vk::ImageLayout::UNDEFINED)
            .new_layout(vk::ImageLayout::GENERAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(pad)
            .subresource_range(color_range(0));
        dev.cmd_pipeline_barrier2(
            compute_cmd,
            &vk::DependencyInfo::default().image_memory_barriers(&[src_acq, pad_dst]),
        );
        let region =
            |aspect: vk::ImageAspectFlags, sx: i32, sy: i32, dx: i32, dy: i32, cw: u32, ch: u32| {
                let layers = vk::ImageSubresourceLayers::default()
                    .aspect_mask(aspect)
                    .layer_count(1);
                vk::ImageCopy::default()
                    .src_subresource(layers)
                    .dst_subresource(layers)
                    .src_offset(vk::Offset3D { x: sx, y: sy, z: 0 })
                    .dst_offset(vk::Offset3D { x: dx, y: dy, z: 0 })
                    .extent(vk::Extent3D {
                        width: cw,
                        height: ch,
                        depth: 1,
                    })
            };
        // Pass 1: visible region, then each bottom padding row as a copy of the last visible row.
        let mut regions = Vec::new();
        for &(aspect, div) in planes {
            let (rw, rh, h) = (rw / div, rh / div, h / div);
            regions.push(region(aspect, 0, 0, 0, 0, rw, rh));
            for y in rh..h {
                regions.push(region(aspect, 0, rh as i32 - 1, 0, y as i32, rw, 1));
            }
        }
        dev.cmd_copy_image(
            compute_cmd,
            src,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            pad,
            vk::ImageLayout::GENERAL,
            &regions,
        );
        // Pass 2: duplicate last visible column over the full aligned height (valid after
        // pass 1 filled the bottom rows). Self-copy in GENERAL; W→R barrier between.
        if w > rw {
            let self_dep = vk::ImageMemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::ALL_TRANSFER)
                .src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
                .dst_stage_mask(vk::PipelineStageFlags2::ALL_TRANSFER)
                .dst_access_mask(vk::AccessFlags2::TRANSFER_READ)
                .old_layout(vk::ImageLayout::GENERAL)
                .new_layout(vk::ImageLayout::GENERAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(pad)
                .subresource_range(color_range(0));
            dev.cmd_pipeline_barrier2(
                compute_cmd,
                &vk::DependencyInfo::default().image_memory_barriers(&[self_dep]),
            );
            let mut cols = Vec::new();
            for &(aspect, div) in planes {
                let (rw, w, h) = (rw / div, w / div, h / div);
                for x in rw..w {
                    cols.push(region(aspect, rw as i32 - 1, 0, x as i32, 0, 1, h));
                }
            }
            dev.cmd_copy_image(
                compute_cmd,
                pad,
                vk::ImageLayout::GENERAL,
                pad,
                vk::ImageLayout::GENERAL,
                &cols,
            );
        }
        if self.ts_period_ns > 0.0 {
            dev.cmd_write_timestamp2(
                compute_cmd,
                vk::PipelineStageFlags2::ALL_COMMANDS,
                ts_pool,
                1,
            );
        }
        // Last read of the import in this batch: release it back to the producer family.
        let src_rel = imported_release_barrier(
            src,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            self.compute_family,
            self.foreign_qfi,
            vk::PipelineStageFlags2::ALL_TRANSFER,
            vk::AccessFlags2::TRANSFER_READ,
        );
        dev.cmd_pipeline_barrier2(
            compute_cmd,
            &vk::DependencyInfo::default().image_memory_barriers(&[src_rel]),
        );
        dev.end_command_buffer(compute_cmd)?;
        Ok(())
    }

    /// Record the CSC into the slot's open compute batch and end it: the host's reframe when
    /// set, the RGB → 4:2:0 dispatch with the cursor blend, the copies into the picture's
    /// planes unless the CSC writes them directly, the imported source's release. The picture
    /// is the encode source.
    unsafe fn record_csc(&self, slot: usize, csc: &CscSource) -> Result<EncodeSource> {
        let dev = &self.device;
        let (w, h_px) = (self.width, self.height);
        let compute_cmd = self.frames[slot].compute_cmd;
        let ts_pool = self.frames[slot].ts_pool;
        let csc_set = self.frames[slot].csc_set;
        let y_img = self.frames[slot].y.image;
        let uv_img = self.frames[slot].uv.image;
        let nv12_src = self.frames[slot].nv12.image;
        let nv12_view = self.frames[slot].nv12.view;
        let direct_planes = self.frames[slot].direct_planes;
        // A reframed session converts the scaled crop; the cursor went in at source size, and
        // the reframe is what read a LINEAR source's words.
        let idle = self.words.idle();
        let (cursor_pc, rgb_view, lin) = match &self.reframe {
            Some(r) => (
                [0; 4],
                r.record(
                    dev,
                    compute_cmd,
                    slot,
                    self.sampler,
                    csc.rgb_view,
                    self.frames[slot].cursor.img.view,
                    csc.cursor_pc,
                    csc.lin,
                    idle,
                ),
                None,
            ),
            None => (csc.cursor_pc, csc.rgb_view, csc.lin),
        };
        bind_rgb(
            dev,
            self.sampler,
            csc_set,
            rgb_view,
            lin.map_or(idle, |l| l.buffer),
        );

        // GENERAL for the CSC's targets, prior contents discarded: the picture itself when its
        // planes are written directly, else Y/UV scratch plus the picture as the copies' dst.
        let to_general = |img, dst_stage, dst_access| {
            vk::ImageMemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::NONE)
                .src_access_mask(vk::AccessFlags2::NONE)
                .dst_stage_mask(dst_stage)
                .dst_access_mask(dst_access)
                .old_layout(vk::ImageLayout::UNDEFINED)
                .new_layout(vk::ImageLayout::GENERAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(img)
                .subresource_range(color_range(0))
        };
        let pre = if direct_planes {
            vec![to_general(
                nv12_src,
                vk::PipelineStageFlags2::COMPUTE_SHADER,
                vk::AccessFlags2::SHADER_WRITE,
            )]
        } else {
            vec![
                to_general(
                    y_img,
                    vk::PipelineStageFlags2::COMPUTE_SHADER,
                    vk::AccessFlags2::SHADER_WRITE,
                ),
                to_general(
                    uv_img,
                    vk::PipelineStageFlags2::COMPUTE_SHADER,
                    vk::AccessFlags2::SHADER_WRITE,
                ),
                to_general(
                    nv12_src,
                    vk::PipelineStageFlags2::ALL_TRANSFER,
                    vk::AccessFlags2::TRANSFER_WRITE,
                ),
            ]
        };
        dev.cmd_pipeline_barrier2(
            compute_cmd,
            &vk::DependencyInfo::default().image_memory_barriers(&pre),
        );

        dev.cmd_bind_pipeline(compute_cmd, vk::PipelineBindPoint::COMPUTE, self.csc_pipe);
        dev.cmd_bind_descriptor_sets(
            compute_cmd,
            vk::PipelineBindPoint::COMPUTE,
            self.csc_layout,
            0,
            &[csc_set],
            &[],
        );
        dev.cmd_push_constants(
            compute_cmd,
            self.csc_layout,
            vk::ShaderStageFlags::COMPUTE,
            0,
            &csc_push(cursor_pc, lin),
        );
        dev.cmd_dispatch(compute_cmd, (w / 2).div_ceil(8), (h_px / 2).div_ceil(8), 1);

        if !direct_planes {
            self.record_plane_copies(compute_cmd, y_img, uv_img, nv12_src);
        }
        if self.ts_period_ns > 0.0 {
            dev.cmd_write_timestamp2(
                compute_cmd,
                vk::PipelineStageFlags2::ALL_COMMANDS,
                ts_pool,
                1,
            );
        }
        // Hand the imported source back to the producer family: the slot fence covers this
        // batch, so the producer may rewrite the dmabuf once the fence signals.
        if let Some(img) = csc.imported {
            let rel = imported_release_barrier(
                img,
                vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                self.compute_family,
                self.foreign_qfi,
                vk::PipelineStageFlags2::COMPUTE_SHADER,
                vk::AccessFlags2::SHADER_READ,
            );
            dev.cmd_pipeline_barrier2(
                compute_cmd,
                &vk::DependencyInfo::default().image_memory_barriers(&[rel]),
            );
        }
        dev.end_command_buffer(compute_cmd)?;
        Ok(EncodeSource {
            img: nv12_src,
            view: nv12_view,
            acquire: SrcAcquire::CscGeneral,
            compute: true,
        })
    }

    /// Copy the CSC's Y/UV scratch into the picture's two planes once the shader writes are
    /// readable. Every image stays GENERAL.
    unsafe fn record_plane_copies(
        &self,
        compute_cmd: vk::CommandBuffer,
        y_img: vk::Image,
        uv_img: vk::Image,
        nv12_src: vk::Image,
    ) {
        let dev = &self.device;
        let (w, h_px) = (self.width, self.height);
        let yuv_rd = |img| {
            vk::ImageMemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::COMPUTE_SHADER)
                .src_access_mask(vk::AccessFlags2::SHADER_WRITE)
                .dst_stage_mask(vk::PipelineStageFlags2::ALL_TRANSFER)
                .dst_access_mask(vk::AccessFlags2::TRANSFER_READ)
                .old_layout(vk::ImageLayout::GENERAL)
                .new_layout(vk::ImageLayout::GENERAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(img)
                .subresource_range(color_range(0))
        };
        dev.cmd_pipeline_barrier2(
            compute_cmd,
            &vk::DependencyInfo::default().image_memory_barriers(&[yuv_rd(y_img), yuv_rd(uv_img)]),
        );
        let plane_copy = |src_aspect, dst_aspect, ew, eh| {
            vk::ImageCopy::default()
                .src_subresource(
                    vk::ImageSubresourceLayers::default()
                        .aspect_mask(src_aspect)
                        .layer_count(1),
                )
                .dst_subresource(
                    vk::ImageSubresourceLayers::default()
                        .aspect_mask(dst_aspect)
                        .layer_count(1),
                )
                .extent(vk::Extent3D {
                    width: ew,
                    height: eh,
                    depth: 1,
                })
        };
        dev.cmd_copy_image(
            compute_cmd,
            y_img,
            vk::ImageLayout::GENERAL,
            nv12_src,
            vk::ImageLayout::GENERAL,
            &[plane_copy(
                vk::ImageAspectFlags::COLOR,
                vk::ImageAspectFlags::PLANE_0,
                w,
                h_px,
            )],
        );
        dev.cmd_copy_image(
            compute_cmd,
            uv_img,
            vk::ImageLayout::GENERAL,
            nv12_src,
            vk::ImageLayout::GENERAL,
            &[plane_copy(
                vk::ImageAspectFlags::COLOR,
                vk::ImageAspectFlags::PLANE_1,
                w / 2,
                h_px / 2,
            )],
        );
    }

    /// Record the slot's encode batch for the session's codec.
    unsafe fn record_coding(
        &self,
        slot: usize,
        src: &EncodeSource,
        plan: &CodingPlan,
    ) -> Result<()> {
        let cmd = self.frames[slot].cmd;
        let query_pool = self.frames[slot].query_pool;
        let bs_buf = self.frames[slot].bs.buffer;
        if self.spec.codec == Codec::Av1 {
            self.record_coding_av1(
                &self.device,
                cmd,
                query_pool,
                bs_buf,
                src.img,
                src.view,
                src.acquire,
                plan.is_idr,
                plan.recovery,
                plan.ref_slot,
                plan.setup_idx,
                plan.poc,
            )
        } else {
            self.record_coding_h265(
                &self.device,
                cmd,
                query_pool,
                bs_buf,
                src.img,
                src.view,
                src.acquire,
                plan.is_idr,
                plan.ref_slot,
                plan.setup_idx,
                plan.poc,
            )
        }
    }

    /// Queue the slot's batches. With a compute batch, compute signals `csc_sem` and the encode
    /// waits it; the encode signals the slot's fence. Per-slot cmd/sem/fence make ring frames
    /// independent; the DPB barrier orders N's reconstruct-write before N+1's read.
    unsafe fn submit(&mut self, slot: usize, compute: bool) -> Result<()> {
        let compute_cmd = self.frames[slot].compute_cmd;
        let cmd = self.frames[slot].cmd;
        let csc_sem = self.frames[slot].csc_sem;
        let fence = self.frames[slot].fence;
        self.device.reset_fences(&[fence])?;
        let ecmds = [cmd];
        if !compute {
            self.device.queue_submit(
                self.encode_queue,
                &[vk::SubmitInfo::default().command_buffers(&ecmds)],
                fence,
            )?;
            return Ok(());
        }
        let ccmds = [compute_cmd];
        let sems = [csc_sem];
        self.device.queue_submit(
            self.compute_queue,
            &[vk::SubmitInfo::default()
                .command_buffers(&ccmds)
                .signal_semaphores(&sems)],
            vk::Fence::null(),
        )?;
        let wait_stages = [vk::PipelineStageFlags::ALL_COMMANDS];
        if let Err(e) = self.device.queue_submit(
            self.encode_queue,
            &[vk::SubmitInfo::default()
                .command_buffers(&ecmds)
                .wait_semaphores(&sems)
                .wait_dst_stage_mask(&wait_stages)],
            fence,
        ) {
            // The compute signal is consumed on a failed encode submit: the queued compute
            // batch signals `csc_sem` with no waiter.
            self.encode_submit_failed(slot);
            return Err(e.into());
        }
        Ok(())
    }

    /// Stash metadata `read_slot` needs once the fence signals, then advance DPB/GOP
    /// bookkeeping and the wave to what `plan` leaves for the next frame.
    fn post_submit_bookkeeping(
        &mut self,
        slot: usize,
        pts_ns: u64,
        wire: i64,
        plan: FramePlan,
        setup_idx: usize,
        poc: i32,
    ) {
        let is_idr = plan.refs == FrameRef::Idr;
        self.frames[slot].pts_ns = pts_ns;
        self.frames[slot].keyframe = is_idr;
        self.frames[slot].recovery_anchor = matches!(plan.refs, FrameRef::Anchor | FrameRef::Acked);
        // The wave's start and close are the client's two marks. Every wave picture but the
        // close is part dirty, so it never becomes an RFI anchor: `slot_wire` stays -1.
        self.frames[slot].recovery_point = plan.mark.point();
        self.frames[slot].recovery_close = plan.mark.close();
        if is_idr {
            self.slot_wire.iter_mut().for_each(|s| *s = -1);
            self.slot_poc.iter_mut().for_each(|s| *s = -1);
        }
        self.slot_wire[setup_idx] = if plan.dirty { -1 } else { wire };
        self.slot_poc[setup_idx] = poc;
        let slot_wire = &self.slot_wire;
        self.kept
            .step(wire, !plan.dirty, self.confirmed.as_ref(), |w| {
                slot_wire.contains(&w)
            });
        if let Some(w) = plan.wave.filter(|w| w.closes()) {
            tracing::debug!(
                cycle = w.cycle,
                wire,
                "vulkan-encode: intra refresh wave closed"
            );
        }
        self.wave = plan.next.wave;
        self.prev_slot = setup_idx;
        self.poc = poc + 1;
        self.enc_count += 1;
        // RESET-install folds on `first_frame`, not `is_idr`: a mid-stream IDR still uses the
        // standalone ENCODE_RATE_CONTROL path.
        let was_first_frame = self.first_frame;
        self.first_frame = false;
        // Session now has a non-default RC mode. Unlike `first_frame`, this survives `reset()`.
        self.rc_installed = true;
        self.force_kf = false;
        if let Some(nb) = self.pending_bitrate.take() {
            // Retarget is recorded; later begins declare the new rate.
            self.bitrate = nb;
            tracing::debug!(
                mbps = nb / 1_000_000,
                folded_into_reset = was_first_frame,
                "vulkan-encode: rate control retargeted"
            );
        }
    }

    /// Begin `cmd` and record shared pre-encode setup: query-pool reset, source acquire
    /// ([`SrcAcquire`]), DPB transition. First frame: whole-image UNDEFINED → DPB. Afterwards:
    /// pipelining barrier ordering the previous reconstruct-write before this reference
    /// read/write (ring records N+1 while N still encodes).
    unsafe fn begin_encode_cmd(
        &self,
        dev: &ash::Device,
        cmd: vk::CommandBuffer,
        query_pool: vk::QueryPool,
        src_img: vk::Image,
        acquire: SrcAcquire,
    ) -> Result<()> {
        dev.begin_command_buffer(
            cmd,
            &vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
        )?;
        dev.cmd_reset_query_pool(cmd, query_pool, 0, 1);
        let dpb_barrier = if self.first_frame {
            vk::ImageMemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::NONE)
                .src_access_mask(vk::AccessFlags2::NONE)
                .dst_stage_mask(vk::PipelineStageFlags2::VIDEO_ENCODE_KHR)
                .dst_access_mask(vk::AccessFlags2::VIDEO_ENCODE_WRITE_KHR)
                .old_layout(vk::ImageLayout::UNDEFINED)
                .new_layout(vk::ImageLayout::VIDEO_ENCODE_DPB_KHR)
        } else {
            vk::ImageMemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::VIDEO_ENCODE_KHR)
                .src_access_mask(vk::AccessFlags2::VIDEO_ENCODE_WRITE_KHR)
                .dst_stage_mask(vk::PipelineStageFlags2::VIDEO_ENCODE_KHR)
                .dst_access_mask(
                    vk::AccessFlags2::VIDEO_ENCODE_READ_KHR
                        | vk::AccessFlags2::VIDEO_ENCODE_WRITE_KHR,
                )
                .old_layout(vk::ImageLayout::VIDEO_ENCODE_DPB_KHR)
                .new_layout(vk::ImageLayout::VIDEO_ENCODE_DPB_KHR)
        }
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(self.dpb.image)
        .subresource_range(vk::ImageSubresourceRange {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            base_mip_level: 0,
            level_count: 1,
            base_array_layer: 0,
            layer_count: DPB_SLOTS,
        });
        let src_base = vk::ImageMemoryBarrier2::default()
            .dst_stage_mask(vk::PipelineStageFlags2::VIDEO_ENCODE_KHR)
            .dst_access_mask(vk::AccessFlags2::VIDEO_ENCODE_READ_KHR)
            .new_layout(vk::ImageLayout::VIDEO_ENCODE_SRC_KHR)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(src_img)
            .subresource_range(color_range(0));
        let src_barrier = match acquire {
            SrcAcquire::CscGeneral => src_base
                .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                .src_access_mask(vk::AccessFlags2::NONE)
                .old_layout(vk::ImageLayout::GENERAL),
            // Imported EXCLUSIVE image: encode must acquire it from the producer family on
            // every use (UNDEFINED on first import; GENERAL after the previous release).
            SrcAcquire::DmabufFresh | SrcAcquire::DmabufCached => imported_acquire_barrier(
                src_img,
                matches!(acquire, SrcAcquire::DmabufFresh),
                self.foreign_qfi,
                self.encode_family,
                vk::PipelineStageFlags2::VIDEO_ENCODE_KHR,
                vk::AccessFlags2::VIDEO_ENCODE_READ_KHR,
                vk::ImageLayout::VIDEO_ENCODE_SRC_KHR,
            ),
            SrcAcquire::CpuUpload => src_base
                .src_stage_mask(vk::PipelineStageFlags2::NONE)
                .src_access_mask(vk::AccessFlags2::NONE)
                .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL),
        };
        let pre_enc = [src_barrier, dpb_barrier];
        dev.cmd_pipeline_barrier2(
            cmd,
            &vk::DependencyInfo::default().image_memory_barriers(&pre_enc),
        );
        Ok(())
    }

    /// Release a direct DMA-BUF source to the foreign producer family. Internal CSC and
    /// CPU-upload sources stay device-owned. The command buffer's slot fence covers the hand-off.
    unsafe fn release_direct_source(
        &self,
        dev: &ash::Device,
        cmd: vk::CommandBuffer,
        src_img: vk::Image,
        acquire: SrcAcquire,
    ) {
        if !matches!(acquire, SrcAcquire::DmabufFresh | SrcAcquire::DmabufCached) {
            return;
        }
        let release = imported_release_barrier(
            src_img,
            vk::ImageLayout::VIDEO_ENCODE_SRC_KHR,
            self.encode_family,
            self.foreign_qfi,
            vk::PipelineStageFlags2::VIDEO_ENCODE_KHR,
            vk::AccessFlags2::VIDEO_ENCODE_READ_KHR,
        );
        dev.cmd_pipeline_barrier2(
            cmd,
            &vk::DependencyInfo::default().image_memory_barriers(&[release]),
        );
    }

    /// `(coded, source)` extents. Coded is the aligned size for app-aligned sessions (pairs
    /// with the aligned SPS) and the render size for native NV12's true-size headers. RGB
    /// true-extent passes the render size as the source: RADV derives firmware padding from
    /// `srcPictureResource.codedExtent`.
    fn coding_extents(&self) -> (vk::Extent2D, vk::Extent2D) {
        let render = vk::Extent2D {
            width: self.spec.width,
            height: self.spec.height,
        };
        let coded = if self.spec.native_nv12 {
            render
        } else {
            vk::Extent2D {
                width: self.width,
                height: self.height,
            }
        };
        let src = if self.rgb.as_ref().is_some_and(|r| r.true_extent) {
            render
        } else {
            coded
        };
        (coded, src)
    }

    /// One rate-control layer at `bps` (average and peak alike) at the session frame rate, the
    /// codec's own layer struct chained at `codec_layer`. NVIDIA fails the encode without it.
    fn rc_layer(
        &self,
        bps: u64,
        codec_layer: *const c_void,
    ) -> [vk::VideoEncodeRateControlLayerInfoKHR<'static>; 1] {
        let mut layer = vk::VideoEncodeRateControlLayerInfoKHR::default()
            .average_bitrate(bps)
            .max_bitrate(bps)
            .frame_rate_numerator(self.fps)
            .frame_rate_denominator(1);
        layer.p_next = codec_layer;
        [layer]
    }

    /// Rate control over `layer`, the codec's own RC struct chained at `codec_rc` by hand
    /// (`push_next` would clobber `rc.p_next`).
    fn rc_info<'a>(
        &self,
        layer: &'a [vk::VideoEncodeRateControlLayerInfoKHR<'a>],
        codec_rc: *const c_void,
    ) -> vk::VideoEncodeRateControlInfoKHR<'a> {
        let mut rc = vk::VideoEncodeRateControlInfoKHR::default()
            .rate_control_mode(self.rc_mode)
            .layers(layer)
            .virtual_buffer_size_in_ms(self.vbv_ms.0)
            .initial_virtual_buffer_size_in_ms(self.vbv_ms.1);
        rc.p_next = codec_rc;
        rc
    }

    /// Begin, the first-frame and retarget controls, encode, end: what both codecs record once
    /// their Std structs exist. `codec_rc`, `codec_rc_layer` and `codec_pic` are the codec's
    /// rate-control, rate-control layer and picture-info structs, chained by address and alive
    /// for this call. `enc_refs` is empty on an IDR. A direct import returns to the producer
    /// after coding ends.
    #[allow(clippy::too_many_arguments)]
    unsafe fn record_coding_common(
        &self,
        dev: &ash::Device,
        cmd: vk::CommandBuffer,
        query_pool: vk::QueryPool,
        bs_buf: vk::Buffer,
        src_img: vk::Image,
        src_view: vk::ImageView,
        acquire: SrcAcquire,
        src_extent: vk::Extent2D,
        begin_slots: &[vk::VideoReferenceSlotInfoKHR],
        setup_slot: &vk::VideoReferenceSlotInfoKHR,
        enc_refs: &[vk::VideoReferenceSlotInfoKHR],
        codec_rc: *const c_void,
        codec_rc_layer: *const c_void,
        codec_pic: *const c_void,
        mut ir_info: crate::vk_intra_refresh::VideoEncodeIntraRefreshInfoKHR,
    ) -> Result<()> {
        use crate::vk_intra_refresh as vir;
        // Declares CURRENT state (`self.bitrate`), never a pending retarget (VUID-...-08254).
        let layer = self.rc_layer(self.bitrate, codec_rc_layer);
        let rc = self.rc_info(&layer, codec_rc);

        self.begin_encode_cmd(dev, cmd, query_pool, src_img, acquire)?;
        let mut begin = vk::VideoBeginCodingInfoKHR::default()
            .video_session(self.session)
            .video_session_parameters(self.params)
            .reference_slots(begin_slots);
        // Declare the session's actual RC state, not `!first_frame` (`reset()` re-arms that).
        if self.rc_installed {
            begin.p_next = &rc as *const _ as *const c_void;
        }
        (self.vq_dev.fp().cmd_begin_video_coding_khr)(cmd, &begin);
        if self.first_frame {
            // RESET + RC install + quality. Without ENCODE_QUALITY_LEVEL RADV never sends a
            // VCN preset op. Quality chains ahead of RC (spec: a quality-level change must
            // carry ENCODE_RATE_CONTROL). Pending retarget folds into the install, not
            // `self.bitrate` before recording — begin must declare the old rate after `reset()`.
            let install_layer =
                self.rc_layer(self.pending_bitrate.unwrap_or(self.bitrate), codec_rc_layer);
            let install_rc = self.rc_info(&install_layer, codec_rc);
            let mut q =
                vk::VideoEncodeQualityLevelInfoKHR::default().quality_level(self.quality_level);
            q.p_next = &install_rc as *const _ as *const c_void;
            let mut ctrl = vk::VideoCodingControlInfoKHR::default().flags(
                vk::VideoCodingControlFlagsKHR::RESET
                    | vk::VideoCodingControlFlagsKHR::ENCODE_RATE_CONTROL
                    | vk::VideoCodingControlFlagsKHR::ENCODE_QUALITY_LEVEL,
            );
            ctrl.p_next = &q as *const _ as *const c_void;
            (self.vq_dev.fp().cmd_control_video_coding_khr)(cmd, &ctrl);
        } else if let Some(nb) = self.pending_bitrate {
            // Mid-stream retarget: begin declared CURRENT; this control installs NEW. No RESET.
            let layer2 = self.rc_layer(nb, codec_rc_layer);
            let rc2 = self.rc_info(&layer2, codec_rc);
            let mut ctrl = vk::VideoCodingControlInfoKHR::default()
                .flags(vk::VideoCodingControlFlagsKHR::ENCODE_RATE_CONTROL);
            ctrl.p_next = &rc2 as *const _ as *const c_void;
            (self.vq_dev.fp().cmd_control_video_coding_khr)(cmd, &ctrl);
        }
        dev.cmd_begin_query(cmd, query_pool, 0, vk::QueryControlFlags::empty());
        let src_res = vk::VideoPictureResourceInfoKHR::default()
            .coded_extent(src_extent)
            .image_view_binding(src_view);
        let mut enc = vk::VideoEncodeInfoKHR::default()
            .dst_buffer(bs_buf)
            .dst_buffer_offset(0)
            .dst_buffer_range(self.bs_size)
            .src_picture_resource(src_res)
            .setup_reference_slot(setup_slot);
        if !enc_refs.is_empty() {
            enc = enc.reference_slots(enc_refs);
        }
        enc.p_next = codec_pic;
        if self.wave.is_some() {
            enc.flags |= vk::VideoEncodeFlagsKHR::from_raw(vir::ENCODE_INTRA_REFRESH_BIT);
            ir_info.p_next = enc.p_next;
            enc.p_next = &ir_info as *const _ as *const c_void;
        }
        (self.venc_dev.fp().cmd_encode_video_khr)(cmd, &enc);
        dev.cmd_end_query(cmd, query_pool, 0);
        (self.vq_dev.fp().cmd_end_video_coding_khr)(cmd, &vk::VideoEndCodingInfoKHR::default());
        self.release_direct_source(dev, cmd, src_img, acquire);
        dev.end_command_buffer(cmd)?;
        Ok(())
    }

    /// HEVC Std structs, then [`Self::record_coding_common`]. A recovery anchor is an ordinary
    /// P whose `RefPicList0` names the known-good slot; the full short-term RPS
    /// ([`build_h265_rps_s0`]) keeps all resident DPB pictures alive.
    #[allow(clippy::too_many_arguments)]
    unsafe fn record_coding_h265(
        &self,
        dev: &ash::Device,
        cmd: vk::CommandBuffer,
        query_pool: vk::QueryPool,
        bs_buf: vk::Buffer,
        src_img: vk::Image,
        src_view: vk::ImageView,
        acquire: SrcAcquire,
        is_idr: bool,
        ref_slot: usize,
        setup_idx: usize,
        poc: i32,
    ) -> Result<()> {
        use ash::vk::native as h;
        let (ext2d, src_extent) = self.coding_extents();
        let ref_poc = if is_idr { 0 } else { self.slot_poc[ref_slot] };

        let mut pic_flags: h::StdVideoEncodeH265PictureInfoFlags = std::mem::zeroed();
        pic_flags.set_is_reference(1);
        if is_idr {
            pic_flags.set_IrapPicFlag(1);
        }
        pic_flags.set_pic_output_flag(1);
        let mut std_pic: h::StdVideoEncodeH265PictureInfo = std::mem::zeroed();
        std_pic.flags = pic_flags;
        std_pic.pic_type = if is_idr {
            h::StdVideoH265PictureType_STD_VIDEO_H265_PICTURE_TYPE_IDR
        } else {
            h::StdVideoH265PictureType_STD_VIDEO_H265_PICTURE_TYPE_P
        };
        std_pic.PicOrderCntVal = poc;
        let (num_neg, deltas, used) = build_h265_rps_s0(&self.slot_poc, setup_idx, ref_poc, poc);
        // `ref_slot` is always resident and never the setup slot; a miss means DPB desynced.
        debug_assert!(is_idr || used != 0, "reference POC missing from the RPS");
        let mut rps: h::StdVideoH265ShortTermRefPicSet = std::mem::zeroed();
        rps.num_negative_pics = num_neg;
        rps.delta_poc_s0_minus1 = deltas;
        rps.used_by_curr_pic_s0_flag = used;
        let mut ref_lists: h::StdVideoEncodeH265ReferenceListsInfo = std::mem::zeroed();
        ref_lists.RefPicList0 = [0xff; 15];
        ref_lists.RefPicList1 = [0xff; 15];
        ref_lists.RefPicList0[0] = ref_slot as u8;
        if !is_idr {
            std_pic.pShortTermRefPicSet = &rps;
            std_pic.pRefLists = &ref_lists;
        }
        let mut sh_flags: h::StdVideoEncodeH265SliceSegmentHeaderFlags = std::mem::zeroed();
        sh_flags.set_first_slice_segment_in_pic_flag(1);
        sh_flags.set_slice_loop_filter_across_slices_enabled_flag(1);
        let mut std_sh: h::StdVideoEncodeH265SliceSegmentHeader = std::mem::zeroed();
        std_sh.flags = sh_flags;
        std_sh.slice_type = if is_idr {
            h::StdVideoH265SliceType_STD_VIDEO_H265_SLICE_TYPE_I
        } else {
            h::StdVideoH265SliceType_STD_VIDEO_H265_SLICE_TYPE_P
        };
        std_sh.MaxNumMergeCand = 5;
        let slice = vk::VideoEncodeH265NaluSliceSegmentInfoKHR::default()
            .constant_qp(0)
            .std_slice_segment_header(&std_sh);
        let slices = [slice];
        let h265_pic = vk::VideoEncodeH265PictureInfoKHR::default()
            .nalu_slice_segment_entries(&slices)
            .std_picture_info(&std_pic);

        let setup_res = vk::VideoPictureResourceInfoKHR::default()
            .coded_extent(ext2d)
            .image_view_binding(self.dpb_views[setup_idx]);
        let mut setup_std: h::StdVideoEncodeH265ReferenceInfo = std::mem::zeroed();
        setup_std.pic_type = std_pic.pic_type;
        setup_std.PicOrderCntVal = poc;
        let mut setup_dpb_a =
            vk::VideoEncodeH265DpbSlotInfoKHR::default().std_reference_info(&setup_std);
        let mut setup_dpb_b =
            vk::VideoEncodeH265DpbSlotInfoKHR::default().std_reference_info(&setup_std);
        let setup_slot = vk::VideoReferenceSlotInfoKHR::default()
            .slot_index(setup_idx as i32)
            .picture_resource(&setup_res)
            .push_next(&mut setup_dpb_a);
        let begin_setup = vk::VideoReferenceSlotInfoKHR::default()
            .slot_index(-1)
            .picture_resource(&setup_res)
            .push_next(&mut setup_dpb_b);

        let ref_res = vk::VideoPictureResourceInfoKHR::default()
            .coded_extent(ext2d)
            .image_view_binding(self.dpb_views[ref_slot]);
        let mut ref_std: h::StdVideoEncodeH265ReferenceInfo = std::mem::zeroed();
        ref_std.pic_type = if ref_poc == 0 {
            h::StdVideoH265PictureType_STD_VIDEO_H265_PICTURE_TYPE_IDR
        } else {
            h::StdVideoH265PictureType_STD_VIDEO_H265_PICTURE_TYPE_P
        };
        ref_std.PicOrderCntVal = ref_poc;
        let mut ref_dpb_a =
            vk::VideoEncodeH265DpbSlotInfoKHR::default().std_reference_info(&ref_std);
        let mut ref_dpb_b =
            vk::VideoEncodeH265DpbSlotInfoKHR::default().std_reference_info(&ref_std);
        // Wave frame: the reference is the previous frame, `cycle - index` of its regions
        // still dirty (VUID-10843). Absent = 0, which every non-wave frame requires.
        // `&mut`: `push_next` below writes `ref_ir.p_next` through the chain.
        let (ir_info, mut ref_ir) = intra_refresh_chain(self.wave);
        if self.wave.is_some() {
            ref_dpb_b.p_next = &mut ref_ir as *mut _ as *const c_void;
        }
        let ref_begin = vk::VideoReferenceSlotInfoKHR::default()
            .slot_index(ref_slot as i32)
            .picture_resource(&ref_res)
            .push_next(&mut ref_dpb_a);
        let ref_enc = vk::VideoReferenceSlotInfoKHR::default()
            .slot_index(ref_slot as i32)
            .picture_resource(&ref_res)
            .push_next(&mut ref_dpb_b);
        let begin_p = [ref_begin, begin_setup];
        let begin_i = [begin_setup];
        let enc_refs = [ref_enc];

        let h265_rc = vk::VideoEncodeH265RateControlInfoKHR::default()
            .flags(vk::VideoEncodeH265RateControlFlagsKHR::REGULAR_GOP)
            .gop_frame_count(u32::MAX)
            .idr_period(u32::MAX)
            .consecutive_b_frame_count(0)
            .sub_layer_count(1);
        // No QP bounds: the driver's rate control alone.
        let h265_rc_layer = vk::VideoEncodeH265RateControlLayerInfoKHR::default();
        let begin_slots: &[vk::VideoReferenceSlotInfoKHR] =
            if is_idr { &begin_i } else { &begin_p };
        let enc_refs: &[vk::VideoReferenceSlotInfoKHR] = if is_idr { &[] } else { &enc_refs };
        self.record_coding_common(
            dev,
            cmd,
            query_pool,
            bs_buf,
            src_img,
            src_view,
            acquire,
            src_extent,
            begin_slots,
            &setup_slot,
            enc_refs,
            &h265_rc as *const _ as *const c_void,
            &h265_rc_layer as *const _ as *const c_void,
            &h265_pic as *const _ as *const c_void,
            ir_info,
        )
    }

    /// AV1 Std structs, then [`Self::record_coding_common`]. IDR, recovery and a wave start
    /// break the CDF chain; a normal P inherits context through `ref_slot`. Virtual slots
    /// persist until refreshed.
    #[allow(clippy::too_many_arguments)]
    unsafe fn record_coding_av1(
        &self,
        dev: &ash::Device,
        cmd: vk::CommandBuffer,
        query_pool: vk::QueryPool,
        bs_buf: vk::Buffer,
        src_img: vk::Image,
        src_view: vk::ImageView,
        acquire: SrcAcquire,
        is_idr: bool,
        recovery: bool,
        ref_slot: usize,
        setup_idx: usize,
        order: i32,
    ) -> Result<()> {
        use crate::vk_av1_encode as av1;
        use ash::vk::native as h;
        let (_, src_extent) = self.coding_extents();

        let mut tile_flags: h::StdVideoAV1TileInfoFlags = std::mem::zeroed();
        tile_flags.set_uniform_tile_spacing_flag(1);
        let mut tile_info: h::StdVideoAV1TileInfo = std::mem::zeroed();
        tile_info.flags = tile_flags;
        tile_info.TileCols = 1;
        tile_info.TileRows = 1;

        let mut quant: h::StdVideoAV1Quantization = std::mem::zeroed();
        quant.base_q_idx = AV1_BASE_Q_IDX;

        let mut loop_filter: h::StdVideoAV1LoopFilter = std::mem::zeroed();
        // Spec 7.14.1 default_loop_filter_ref_deltas: intra +1, golden/bwd/altref2/altref -1.
        loop_filter.loop_filter_ref_deltas = [1, 0, 0, 0, -1, 0, -1, -1];

        let cdef: h::StdVideoAV1CDEF = std::mem::zeroed();

        let mut lr: h::StdVideoAV1LoopRestoration = std::mem::zeroed();
        lr.FrameRestorationType =
            [h::StdVideoAV1FrameRestorationType_STD_VIDEO_AV1_FRAME_RESTORATION_TYPE_NONE; 3];

        let gm: h::StdVideoAV1GlobalMotion = std::mem::zeroed();

        // Order hints of the 8 physical DPB slots; 0 where empty.
        let mut ref_order_hint = [0u8; 8];
        for (i, &poc) in self.slot_poc.iter().enumerate().take(8) {
            ref_order_hint[i] = poc.max(0) as u8;
        }

        // Recovery/IDR/wave start: error-resilient, no CDF inherit — the client's CDFs for a
        // concealed frame are undefined. Normal P inherits from name 0 → `ref_slot`.
        let independent = is_idr || recovery || self.wave.is_some_and(|w| w.index == 0);
        let mut pic_flags: av1::StdVideoEncodeAV1PictureInfoFlags = std::mem::zeroed();
        pic_flags.set_show_frame(1);
        if independent {
            pic_flags.set_error_resilient_mode(1);
        }
        // AV1 ignores `render_*_minus_1` unless this flag is set. Without it the decoder uses
        // the coded size and displays alignment padding.
        if self.spec.width != self.av1_frame.width || self.spec.height != self.av1_frame.height {
            pic_flags.set_render_and_frame_size_different(1);
        }
        let mut std_pic: av1::StdVideoEncodeAV1PictureInfo = std::mem::zeroed();
        std_pic.flags = pic_flags;
        std_pic.frame_type = if is_idr {
            h::StdVideoAV1FrameType_STD_VIDEO_AV1_FRAME_TYPE_KEY
        } else {
            h::StdVideoAV1FrameType_STD_VIDEO_AV1_FRAME_TYPE_INTER
        };
        std_pic.order_hint = order as u8;
        std_pic.primary_ref_frame = if independent {
            av1::PRIMARY_REF_NONE
        } else {
            0
        };
        std_pic.refresh_frame_flags = if is_idr { 0xff } else { 1u8 << setup_idx };
        std_pic.render_width_minus_1 = (self.spec.width - 1) as u16;
        std_pic.render_height_minus_1 = (self.spec.height - 1) as u16;
        std_pic.interpolation_filter = 0; // EIGHTTAP
        std_pic.TxMode = h::StdVideoAV1TxMode_STD_VIDEO_AV1_TX_MODE_SELECT;
        std_pic.ref_order_hint = ref_order_hint;
        if !is_idr {
            // Every reference name maps to the (recovery or previous) DPB slot.
            std_pic.ref_frame_idx = [ref_slot as i8; 7];
        }
        std_pic.pTileInfo = &tile_info;
        std_pic.pQuantization = &quant;
        std_pic.pLoopFilter = &loop_filter;
        std_pic.pCDEF = &cdef;
        std_pic.pLoopRestoration = &lr;
        // pSegmentation MUST be NULL (VUID-vkCmdEncodeVideoKHR-pStdPictureInfo-10350).
        std_pic.pGlobalMotion = &gm;

        let av1_pic = av1::VideoEncodeAV1PictureInfoKHR {
            s_type: av1::stype(av1::ST_PICTURE_INFO),
            p_next: std::ptr::null(),
            prediction_mode: if is_idr {
                av1::PREDICTION_MODE_INTRA_ONLY
            } else {
                av1::PREDICTION_MODE_SINGLE_REFERENCE
            },
            rate_control_group: if is_idr {
                av1::RC_GROUP_INTRA
            } else {
                av1::RC_GROUP_PREDICTIVE
            },
            // Must be zero when RC is not DISABLED (VUID-...-constantQIndex-10320). Q still
            // reaches the encoder through `pQuantization` for the header.
            constant_q_index: 0,
            p_std_picture_info: &std_pic,
            reference_name_slot_indices: if is_idr {
                [-1; av1::MAX_VIDEO_AV1_REFERENCES_PER_FRAME]
            } else {
                [ref_slot as i32; av1::MAX_VIDEO_AV1_REFERENCES_PER_FRAME]
            },
            primary_reference_cdf_only: 0,
            generate_obu_extension_header: 0,
        };

        // DPB slots carry the SOURCE extent. Without MOTION_VECTOR_SCALING every reference
        // slot's `codedExtent` must equal the source's (VUID-...-flags-10325).
        let setup_res = vk::VideoPictureResourceInfoKHR::default()
            .coded_extent(src_extent)
            .image_view_binding(self.dpb_views[setup_idx]);
        let mut setup_ref_std: av1::StdVideoEncodeAV1ReferenceInfo = std::mem::zeroed();
        setup_ref_std.frame_type = std_pic.frame_type;
        setup_ref_std.OrderHint = order as u8;
        let setup_dpb = av1::VideoEncodeAV1DpbSlotInfoKHR {
            s_type: av1::stype(av1::ST_DPB_SLOT_INFO),
            p_next: std::ptr::null(),
            p_std_reference_info: &setup_ref_std,
        };
        let mut setup_slot = vk::VideoReferenceSlotInfoKHR::default()
            .slot_index(setup_idx as i32)
            .picture_resource(&setup_res);
        setup_slot.p_next = &setup_dpb as *const _ as *const c_void;
        let mut begin_setup = vk::VideoReferenceSlotInfoKHR::default()
            .slot_index(-1)
            .picture_resource(&setup_res);
        begin_setup.p_next = &setup_dpb as *const _ as *const c_void;

        let ref_res = vk::VideoPictureResourceInfoKHR::default()
            .coded_extent(src_extent)
            .image_view_binding(self.dpb_views[ref_slot]);
        let mut ref_ref_std: av1::StdVideoEncodeAV1ReferenceInfo = std::mem::zeroed();
        ref_ref_std.frame_type = if self.slot_poc[ref_slot] == 0 {
            h::StdVideoAV1FrameType_STD_VIDEO_AV1_FRAME_TYPE_KEY
        } else {
            h::StdVideoAV1FrameType_STD_VIDEO_AV1_FRAME_TYPE_INTER
        };
        ref_ref_std.OrderHint = self.slot_poc[ref_slot].max(0) as u8;
        let ref_dpb = av1::VideoEncodeAV1DpbSlotInfoKHR {
            s_type: av1::stype(av1::ST_DPB_SLOT_INFO),
            p_next: std::ptr::null(),
            p_std_reference_info: &ref_ref_std,
        };
        let mut ref_begin = vk::VideoReferenceSlotInfoKHR::default()
            .slot_index(ref_slot as i32)
            .picture_resource(&ref_res);
        ref_begin.p_next = &ref_dpb as *const _ as *const c_void;
        // Wave frame: the reference is the previous frame, `cycle - index` of its regions
        // still dirty (VUID-10843). Absent = 0, which every non-wave frame requires.
        let (ir_info, mut ref_ir) = intra_refresh_chain(self.wave);
        ref_ir.p_next = &ref_dpb as *const _ as *const c_void;
        let mut ref_enc = vk::VideoReferenceSlotInfoKHR::default()
            .slot_index(ref_slot as i32)
            .picture_resource(&ref_res);
        ref_enc.p_next = if self.wave.is_some() {
            &mut ref_ir as *mut _ as *const c_void
        } else {
            &ref_dpb as *const _ as *const c_void
        };
        let begin_p = [ref_begin, begin_setup];
        let begin_i = [begin_setup];
        let enc_refs = [ref_enc];

        let av1_rc = av1::VideoEncodeAV1RateControlInfoKHR {
            s_type: av1::stype(av1::ST_RATE_CONTROL_INFO),
            p_next: std::ptr::null(),
            flags: 0,
            gop_frame_count: 0,
            key_frame_period: 0,
            consecutive_bipredictive_frame_count: 0,
            temporal_layer_count: 1,
        };
        // No q-index or frame-size bounds: the driver's rate control alone.
        let mut av1_rc_layer: av1::VideoEncodeAV1RateControlLayerInfoKHR = std::mem::zeroed();
        av1_rc_layer.s_type = av1::stype(av1::ST_RATE_CONTROL_LAYER_INFO);
        let begin_slots: &[vk::VideoReferenceSlotInfoKHR] =
            if is_idr { &begin_i } else { &begin_p };
        let enc_refs: &[vk::VideoReferenceSlotInfoKHR] = if is_idr { &[] } else { &enc_refs };
        self.record_coding_common(
            dev,
            cmd,
            query_pool,
            bs_buf,
            src_img,
            src_view,
            acquire,
            src_extent,
            begin_slots,
            &setup_slot,
            enc_refs,
            &av1_rc as *const _ as *const c_void,
            &av1_rc_layer as *const _ as *const c_void,
            &av1_pic as *const _ as *const c_void,
            ir_info,
        )
    }
}
