//! A small pool of GBM-allocated dmabufs, for capture protocols where the *client*
//! supplies the buffers (`ext-image-copy-capture-v1`), unlike PipeWire where the
//! producer allocates, and the [`FreeList`] an encoder's hold returns them to.
//!
//! Only `libgbm` — already linked for the EGL importer — plus a render-node open.
//! No new build dependency: every Mesa/NVIDIA install ships it.

use anyhow::{bail, Context, Result};
use pf_zerocopy::gbm::{GbmBo, GbmDevice, GBM_BO_USE_RENDERING, GBM_BO_USE_SCANOUT};
use std::os::fd::{AsRawFd, OwnedFd};
use std::rc::Rc;
use std::sync::{Arc, Mutex};

/// Render node whose `rdev` matches the `dev_t` a compositor advertised.
///
/// The protocol names the device by number, not path. Scanning `/dev/dri` is the only
/// mapping; `drmGetDeviceNameFromFd2` would need libdrm for one lookup.
pub(super) fn render_node_for(dev: u64) -> Result<std::fs::File> {
    let dir = std::fs::read_dir("/dev/dri").context("open /dev/dri")?;
    for e in dir.flatten() {
        let path = e.path();
        let Ok(md) = std::fs::metadata(&path) else {
            continue;
        };
        use std::os::unix::fs::MetadataExt;
        if md.rdev() == dev {
            return std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .with_context(|| format!("open {}", path.display()));
        }
    }
    bail!("no /dev/dri node with device id {dev:#x} — the compositor named a device this host cannot open")
}

/// GBM buffers the compositor writes and the encoder reads. Each keeps the device alive.
pub(super) struct GbmPool {
    pub(super) bos: Vec<GbmBo>,
}

// SAFETY: the gbm handles, and the `Rc<GbmDevice>` every bo shares, are created, read and
// destroyed only on the capture thread that owns the pool. `Send` exists so the whole pool
// moves there once at construction; nothing published carries a gbm pointer or an `Rc`
// clone — a frame gets a `dup` of a bo's fd, a plain owned fd with no thread affinity.
unsafe impl Send for GbmPool {}

impl GbmPool {
    /// Allocate `count` buffers of `width`×`height` in `fourcc`.
    ///
    /// `modifiers` is what the compositor offered, already narrowed to what the consumer
    /// can import. An empty list (or an allocation that refuses every one) falls back to
    /// the implicit modifier, which is what a LINEAR-only path needs.
    pub(super) fn new(
        node: std::fs::File,
        width: u32,
        height: u32,
        fourcc: u32,
        modifiers: &[u64],
        count: usize,
    ) -> Result<GbmPool> {
        let device = Rc::new(
            GbmDevice::open(node).context("open the GBM device on the compositor's render node")?,
        );
        let bos = (0..count)
            .map(|_| alloc(&device, width, height, fourcc, modifiers))
            .collect::<Result<_>>()?;
        Ok(GbmPool { bos })
    }
}

fn alloc(
    device: &Rc<GbmDevice>,
    width: u32,
    height: u32,
    fourcc: u32,
    modifiers: &[u64],
) -> Result<GbmBo> {
    let bo = GbmBo::alloc(
        device,
        width,
        height,
        fourcc,
        modifiers,
        GBM_BO_USE_RENDERING,
    )
    .or_else(|e| {
        if modifiers.is_empty() {
            return Err(e);
        }
        GbmBo::alloc(device, width, height, fourcc, &[], GBM_BO_USE_RENDERING)
    })?;
    let bo = padded_linear(device, width, height, fourcc, bo);
    // A multi-plane bo cannot travel our single-fd frame contract.
    if bo.planes > 1 {
        bail!(
            "gbm allocated a {}-plane buffer; the frame contract carries one fd",
            bo.planes
        );
    }
    Ok(bo)
}

/// The row pitch every reader of a LINEAR buffer steps as given: iHD rounds an import up to 64
/// bytes and NVIDIA's Vulkan to 32, and a rounded pitch shears the picture.
const LINEAR_PITCH: u32 = 64;

/// `bo`, or a LINEAR buffer with rows on [`LINEAR_PITCH`] in its place. Mesa packs a
/// rendering-only LINEAR buffer at the row width and pads one a plane can scan out; this
/// capture has no CPU lane to de-pad through. A driver that refuses or packs both keeps `bo`.
fn padded_linear(device: &Rc<GbmDevice>, width: u32, height: u32, fourcc: u32, bo: GbmBo) -> GbmBo {
    let packed = |b: &GbmBo| b.modifier == 0 && b.stride % LINEAR_PITCH != 0;
    if !packed(&bo) {
        return bo;
    }
    let flags = GBM_BO_USE_RENDERING | GBM_BO_USE_SCANOUT;
    match GbmBo::alloc(device, width, height, fourcc, &[0], flags) {
        Ok(b) if b.planes == 1 && b.modifier == 0 && !packed(&b) => b,
        _ => bo,
    }
}

/// Which buffers the encoder still holds, and the pipe that wakes the loop when one
/// comes back. A returned buffer must re-arm at once or the capture rate halves.
pub(super) struct FreeList {
    pub(super) free: Mutex<Vec<usize>>,
    /// Written by a dropping [`BufHold`] on whatever thread the encoder used.
    pub(super) wake_w: OwnedFd,
}

impl FreeList {
    pub(super) fn take(&self) -> Option<usize> {
        self.free.lock().ok()?.pop()
    }

    pub(super) fn put(&self, idx: usize) {
        if let Ok(mut f) = self.free.lock() {
            f.push(idx);
        }
        // SAFETY: `wake_w` is this struct's live pipe write end; `write` reads one byte
        // from a local and touches no Rust memory beyond it. A full pipe already has a
        // pending wakeup, so a short write is correct to ignore.
        unsafe {
            let b = 1u8;
            libc::write(self.wake_w.as_raw_fd(), std::ptr::addr_of!(b).cast(), 1);
        }
    }
}

/// Returns its buffer to the pool when the last clone drops.
pub(super) struct BufHold {
    pub(super) list: Arc<FreeList>,
    pub(super) idx: usize,
}

impl Drop for BufHold {
    fn drop(&mut self) {
        self.list.put(self.idx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hardware: a LINEAR buffer at a width whose packed row is off 64 bytes comes back with
    /// padded rows. `PF_GBM_NODE` names the render node; Mesa's Intel driver is the case.
    #[test]
    #[ignore = "requires a Mesa render node that pads scanout buffers (Intel)"]
    fn a_linear_buffer_gets_padded_rows() {
        let node = std::env::var("PF_GBM_NODE").unwrap_or_else(|_| "/dev/dri/renderD128".into());
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&node)
            .expect("open the render node");
        let device = Rc::new(GbmDevice::open(file).expect("gbm device"));
        let xr24 = u32::from_le_bytes(*b"XR24");
        let bo = alloc(&device, 1690, 1056, xr24, &[0]).expect("allocate");
        assert_eq!(bo.modifier, 0);
        assert_eq!(bo.stride % LINEAR_PITCH, 0, "stride {}", bo.stride);
    }
}
