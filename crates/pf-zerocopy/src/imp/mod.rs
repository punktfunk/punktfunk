//! Zero-copy capture→encode: the PipeWire dmabuf is imported into CUDA via EGL
//! and handed to NVENC, so the CPU never copies pixels. VAAPI LINEAR-dmabuf
//! passthrough is the same idea without the EGL hop.
//!
//! `PUNKTFUNK_ZEROCOPY` unset defaults ON. `=0` opts out; `PUNKTFUNK_FORCE_SHM`
//! forces SHM. Explicit `=1` ([`zerocopy_forced`]) keeps a failed negotiation
//! erroring instead of falling back to CPU.
//!
//! Pieces: [`cuda`] (driver-API FFI + shared `CUcontext`), [`egl`] (headless
//! EGLDisplay + dmabuf→`EGLImage`→CUDA). Encoder CUDA frames live in
//! `pf-encode`; dmabuf negotiation lives in `pf-capture`. Isolation:
//! `design/zerocopy-worker-isolation.md`.

pub mod client;
pub mod cuda;
pub mod egl;
pub mod gbm;
// Shared worker rails (SEQPACKET ± `SCM_RIGHTS`, pinned-exe spawn, reaping).
// Message body is generic; `proto` is this worker's vocabulary only.
pub mod ipc;
pub mod proto;
#[cfg(test)]
mod tiled_spike;
pub mod vkdev;
pub mod vkslot;
pub mod vulkan;
pub mod worker;

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

pub use cuda::DeviceBuffer;
pub use egl::{DmabufPlane, EglImporter};
pub use proto::{ConvertOut, ConvertSrc, CursorRect, ImportKind};

/// `(st_dev, st_ino)` of an open fd. Stable across dups and `SCM_RIGHTS` re-numbering; a
/// dma-buf keeps its inode for life, so import caches key on it.
pub fn fd_identity(fd: std::os::fd::BorrowedFd<'_>) -> std::io::Result<(u64, u64)> {
    let st = rustix::fs::fstat(fd)?;
    Ok((st.st_dev as u64, st.st_ino as u64))
}

/// Parse a `PUNKTFUNK_*` boolean. Unrecognised spellings return `None` (the
/// flag's default), not false: `TRUE` as "off" inverted host-wide defaults.
fn flag_opt(name: &str) -> Option<bool> {
    let v = std::env::var(name).ok()?;
    match v.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" | "" => Some(false),
        _ => {
            use std::collections::HashSet;
            use std::sync::Mutex;
            static WARNED: Mutex<Option<HashSet<String>>> = Mutex::new(None);
            let mut g = WARNED.lock().unwrap();
            if g.get_or_insert_with(HashSet::new).insert(name.to_string()) {
                tracing::warn!(
                    flag = name,
                    value = %v,
                    "unrecognised boolean value for this PUNKTFUNK_* flag — expected \
                     1/true/yes/on or 0/false/no/off (any case); using the flag's default"
                );
            }
            None
        }
    }
}

fn flag(name: &str) -> bool {
    flag_opt(name).unwrap_or(false)
}

/// `PUNKTFUNK_ZEROCOPY` is explicitly truthy. Failed negotiation then errors
/// instead of falling back to CPU. Both negotiation latches read this.
pub fn zerocopy_forced() -> bool {
    flag_opt("PUNKTFUNK_ZEROCOPY") == Some(true)
}

/// Zero-copy is on. Unset defaults ON; `PUNKTFUNK_ZEROCOPY=0` opts out.
/// Global env switch only; each capture's [`ZeroCopyHealth`] applies its own latches.
pub fn enabled() -> bool {
    flag_opt("PUNKTFUNK_ZEROCOPY").unwrap_or(true)
}

/// `PUNKTFUNK_NVENC_RAW=0` keeps the NVENC lane on the import path: the capture converts each
/// frame into a CUDA buffer and the encoder copies it into a slot. Default on: the capture
/// hands the encoder the held dmabuf and the worker's fused pass writes the slot directly.
pub fn nvenc_raw_enabled() -> bool {
    flag_opt("PUNKTFUNK_NVENC_RAW").unwrap_or(true)
}

/// Can this box run the fused convert at all? Asks a worker to export the convert timeline,
/// which builds the whole pipeline (modifier import, timeline export, the shader) and so answers
/// the same question the first frame would — before a session is committed to the raw lane.
///
/// Cached: the answer is a property of the driver, not of a session. `false` keeps capture on the
/// import path from the start instead of failing the first session that reaches the convert.
pub fn fused_convert_available() -> bool {
    static OK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *OK.get_or_init(|| {
        if !nvenc_raw_enabled() {
            return false;
        }
        let probe = Importer::new_for_capture().and_then(|mut i| i.convert_timeline().map(|_| ()));
        match probe {
            Ok(()) => true,
            Err(e) => {
                tracing::info!(
                    error = %format!("{e:#}"),
                    "fused convert unavailable on this box — capture keeps the import path"
                );
                false
            }
        }
    })
}

/// GPU RGB→NV12 before NVENC. Default ON: NVENC's internal CSC otherwise
/// runs on the SM the game saturates. `PUNKTFUNK_NV12=0` restores RGB/BGRx.
/// LINEAR (gamescope/Vulkan-bridge) captures ignore this.
pub fn nv12_enabled() -> bool {
    flag_opt("PUNKTFUNK_NV12").unwrap_or(true)
}

/// GPU importer for a capture. Default is the worker subprocess so a driver
/// fault on a producer-invalidated dmabuf kills the worker, not the host
/// (`design/zerocopy-worker-isolation.md`). `PUNKTFUNK_ZEROCOPY_INPROC=1`
/// imports in-process (debug / A-B only).
pub enum Importer {
    Remote(client::RemoteImporter),
    InProc(Box<EglImporter>),
}

impl Importer {
    /// `Err` means no GPU import — callers fall back to CPU.
    pub fn new_for_capture() -> anyhow::Result<Importer> {
        if flag("PUNKTFUNK_ZEROCOPY_INPROC") {
            tracing::warn!(
                "PUNKTFUNK_ZEROCOPY_INPROC=1 — GPU import runs IN-PROCESS; a driver fault on a \
                 dying compositor's dmabuf can take the whole host down (debug/A-B use only)"
            );
            return Ok(Importer::InProc(Box::new(EglImporter::new()?)));
        }
        Ok(Importer::Remote(client::RemoteImporter::spawn()?))
    }

    pub fn supported_modifiers(&mut self, fourcc: u32) -> Vec<u64> {
        match self {
            Importer::Remote(r) => r.supported_modifiers(fourcc),
            Importer::InProc(i) => i.supported_modifiers(fourcc),
        }
    }

    /// One dmabuf → CUDA import as `kind`; see [`EglImporter::import`].
    pub fn import(
        &mut self,
        kind: ImportKind,
        plane: &DmabufPlane,
        width: u32,
        height: u32,
        fourcc: u32,
        modifier: Option<u64>,
    ) -> anyhow::Result<DeviceBuffer> {
        match self {
            Importer::Remote(r) => r.import(kind, plane, width, height, fourcc, modifier),
            Importer::InProc(i) => i.import(kind, plane, width, height, fourcc, modifier),
        }
    }

    /// Always `false` in-process — an in-process driver fault does not return.
    pub fn dead(&self) -> bool {
        match self {
            Importer::Remote(r) => r.dead(),
            Importer::InProc(_) => false,
        }
    }
    /// The fused convert lane: the encoder's NVENC slot, imported once by id.
    pub fn register_slot(
        &mut self,
        id: u32,
        fd: std::os::fd::OwnedFd,
        size: u64,
    ) -> anyhow::Result<()> {
        match self {
            Importer::Remote(r) => r.register_slot(id, std::os::fd::AsFd::as_fd(&fd), size),
            Importer::InProc(i) => i.register_slot(id, fd, size),
        }
    }
    pub fn forget_slots(&mut self) {
        match self {
            Importer::Remote(r) => r.forget_slots(),
            Importer::InProc(i) => i.forget_slots(),
        }
    }
    pub fn set_cursor(
        &mut self,
        serial: u64,
        width: u32,
        height: u32,
        rgba: &[u8],
    ) -> anyhow::Result<()> {
        match self {
            Importer::Remote(r) => r.set_cursor(serial, width, height, rgba),
            Importer::InProc(i) => i.set_cursor(serial, width, height, rgba),
        }
    }
    /// One fused pass: the dmabuf (any modifier) plus the cursor into the registered slot.
    /// Returns the timeline value the pass signals; wait it through
    /// [`convert_timeline`](Self::convert_timeline) before the slot is read.
    pub fn convert(
        &mut self,
        src: &ConvertSrc,
        slot: u32,
        out: &ConvertOut,
        cursor: Option<CursorRect>,
    ) -> anyhow::Result<u64> {
        match self {
            Importer::Remote(r) => r.convert(src, slot, out, cursor),
            Importer::InProc(i) => i.convert(src, slot, out, cursor),
        }
    }
    /// The convert timeline as an OPAQUE_FD, imported into CUDA once per encoder session.
    pub fn convert_timeline(&mut self) -> anyhow::Result<std::os::fd::OwnedFd> {
        match self {
            Importer::Remote(r) => r.convert_timeline(),
            Importer::InProc(i) => i.convert_timeline_fd(),
        }
    }

    /// Drop per-buffer caches. A format renegotiation recycles fd numbers; a
    /// stale import must not resolve.
    pub fn clear_cache(&mut self) {
        match self {
            Importer::Remote(r) => r.clear_cache(),
            Importer::InProc(i) => i.clear_linear_cache(),
        }
    }
}

/// One compositor crash costs one rebuild; three consecutive worker deaths disable
/// the GPU importer for that capture identity.
const GPU_IMPORT_DEATH_LATCH: u32 = 3;

/// Below the encoder rebuild budget, so this fires before that session ends.
const RAW_DMABUF_FAILURE_LATCH: u32 = 3;

/// 2 = one retry. Each failed negotiation is a ~10 s stall; a larger budget
/// is paid in dead air. Same capture identity accumulates, so a compositor
/// that never accepts latches on the second try.
const RAW_DMABUF_NEGOTIATION_LATCH: u32 = 2;

/// Raw-dmabuf passthrough off-switch: two causes, two lifetimes.
///
/// Import failures stay sticky — a driver that will not import this
/// compositor's dmabuf fails identically on every retry. Negotiation
/// timeouts get [`RAW_DMABUF_NEGOTIATION_LATCH`] retries. Both are keyed
/// to a capture identity; a new node id is a different question.
///
/// Atomics: [`note_import_ok`](Self::note_import_ok) is on the per-frame
/// import path.
#[derive(Debug)]
pub struct RawDmabufLatch {
    import_streak: AtomicU32,
    import_latched: AtomicBool,
    negotiation_streak: AtomicU32,
    negotiation_latched: AtomicBool,
}

impl RawDmabufLatch {
    pub const fn new() -> Self {
        RawDmabufLatch {
            import_streak: AtomicU32::new(0),
            import_latched: AtomicBool::new(false),
            negotiation_streak: AtomicU32::new(0),
            negotiation_latched: AtomicBool::new(false),
        }
    }

    pub fn disabled(&self) -> bool {
        self.import_latched.load(Ordering::Relaxed)
            || self.negotiation_latched.load(Ordering::Relaxed)
    }

    pub fn note_import_failure(&self) -> Option<u32> {
        let streak = self.import_streak.fetch_add(1, Ordering::Relaxed) + 1;
        (streak >= RAW_DMABUF_FAILURE_LATCH && !self.import_latched.swap(true, Ordering::Relaxed))
            .then_some(streak)
    }

    /// Reset the failure streak. Does not clear a sticky verdict: after fallback,
    /// this identity has no dmabuf import left to succeed.
    pub fn note_import_ok(&self) {
        self.import_streak.store(0, Ordering::Relaxed);
    }

    pub fn note_negotiation_timeout(&self) -> Option<u32> {
        let streak = self.negotiation_streak.fetch_add(1, Ordering::Relaxed) + 1;
        (streak >= RAW_DMABUF_NEGOTIATION_LATCH
            && !self.negotiation_latched.swap(true, Ordering::Relaxed))
        .then_some(streak)
    }

    /// A success spends none of the consecutive-failure retry budget.
    pub fn note_negotiation_ok(&self) {
        self.negotiation_streak.store(0, Ordering::Relaxed);
    }

    /// Which cause holds it off, for the session-open line.
    pub fn state(&self) -> &'static str {
        match (
            self.import_latched.load(Ordering::Relaxed),
            self.negotiation_latched.load(Ordering::Relaxed),
        ) {
            (true, true) => "latched: encoder-import + negotiation",
            (true, false) => "latched: encoder-import failures",
            (false, true) => "latched: negotiation timeouts",
            (false, false) => "live",
        }
    }
}

impl Default for RawDmabufLatch {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Default)]
struct ZeroCopyHealthState {
    raw: RawDmabufLatch,
    gpu_death_streak: AtomicU32,
    gpu_disabled: AtomicBool,
    gpu_negotiation_failed: AtomicBool,
    passthrough_tiled_refused: AtomicBool,
    hdr_tiled_refused: AtomicBool,
    planar_refused: AtomicBool,
}

/// Failure memory for one capture identity. Clones share only that source's verdicts;
/// concurrent sessions on another node cannot clear or poison them.
#[derive(Clone, Debug)]
pub struct ZeroCopyHealth(std::sync::Arc<ZeroCopyHealthState>);

impl ZeroCopyHealth {
    pub fn raw_disabled(&self) -> bool {
        self.0.raw.disabled()
    }

    /// Record an encoder import failure. Tiled refuses immediately; LINEAR keeps the
    /// three-failure budget. `true` asks the owning capture to rebuild on its safe offer.
    pub fn note_raw_import_failure(&self, modifier: u64, reason: &str) -> bool {
        if modifier != 0 {
            let first = self.refuse_passthrough_tiled();
            self.0.hdr_tiled_refused.store(true, Ordering::Relaxed);
            if first {
                tracing::error!(
                    reason,
                    "tiled raw-dmabuf import rejected for this capture — rebuilding on LINEAR"
                );
            }
            return true;
        }
        let latched = self.0.raw.note_import_failure().is_some();
        if latched {
            tracing::error!(
                reason,
                "LINEAR raw-dmabuf import disabled for this capture after repeated failures"
            );
        }
        latched
    }

    pub fn note_raw_import_ok(&self) {
        self.0.raw.note_import_ok();
    }

    pub fn note_raw_negotiation_failed(&self) {
        match self.0.raw.note_negotiation_timeout() {
            Some(streak) => tracing::warn!(
                streak,
                "zero-copy raw-dmabuf passthrough disabled for this capture after repeated negotiation timeouts"
            ),
            None => tracing::warn!(
                "the compositor did not accept the raw-dmabuf offer — retrying once"
            ),
        }
    }

    pub fn note_raw_negotiation_ok(&self) {
        self.0.raw.note_negotiation_ok();
    }

    pub fn raw_state(&self) -> &'static str {
        self.0.raw.state()
    }

    pub fn note_gpu_import_death(&self) {
        let streak = self.0.gpu_death_streak.fetch_add(1, Ordering::Relaxed) + 1;
        if streak >= GPU_IMPORT_DEATH_LATCH && !self.0.gpu_disabled.swap(true, Ordering::Relaxed) {
            tracing::error!(
                streak,
                "zero-copy GPU import disabled for this capture after repeated worker deaths"
            );
        }
    }

    pub fn note_gpu_import_ok(&self) {
        self.0.gpu_death_streak.store(0, Ordering::Relaxed);
    }

    pub fn gpu_import_disabled(&self) -> bool {
        self.0.gpu_disabled.load(Ordering::Relaxed)
    }

    pub fn note_gpu_negotiation_failed(&self) {
        if !self.0.gpu_negotiation_failed.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                "zero-copy EGL-to-CUDA dmabuf offer disabled for this capture after a negotiation timeout"
            );
        }
    }

    pub fn gpu_negotiation_disabled(&self) -> bool {
        self.0.gpu_negotiation_failed.load(Ordering::Relaxed)
    }

    pub fn passthrough_tiled_refused(&self) -> bool {
        self.0.passthrough_tiled_refused.load(Ordering::Relaxed)
    }

    pub fn refuse_passthrough_tiled(&self) -> bool {
        !self
            .0
            .passthrough_tiled_refused
            .swap(true, Ordering::Relaxed)
    }

    pub fn hdr_tiled_refused(&self) -> bool {
        self.0.hdr_tiled_refused.load(Ordering::Relaxed)
    }

    pub fn refuse_hdr_tiled(&self) -> bool {
        !self.0.hdr_tiled_refused.swap(true, Ordering::Relaxed)
    }

    /// The producer's NV12/P010 offer is withdrawn: the CPU de-pad reads one packed plane,
    /// so a planar frame the raw passthrough declines has no lane at all.
    pub fn planar_refused(&self) -> bool {
        self.0.planar_refused.load(Ordering::Relaxed)
    }

    pub fn refuse_planar(&self) {
        self.0.planar_refused.store(true, Ordering::Relaxed);
    }
}

/// Keep recent capture verdicts sticky across rebuilds without an unbounded node-id map.
pub fn zero_copy_health(identity: u64) -> ZeroCopyHealth {
    const CAP: usize = 64;
    static REGISTRY: std::sync::OnceLock<
        std::sync::Mutex<std::collections::VecDeque<(u64, ZeroCopyHealth)>>,
    > = std::sync::OnceLock::new();
    let mut registry = REGISTRY
        .get_or_init(|| std::sync::Mutex::new(std::collections::VecDeque::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(pos) = registry.iter().position(|(id, _)| *id == identity) {
        let entry = registry.remove(pos).expect("position exists");
        let health = entry.1.clone();
        registry.push_back(entry);
        return health;
    }
    let health = ZeroCopyHealth(std::sync::Arc::new(ZeroCopyHealthState::default()));
    registry.push_back((identity, health.clone()));
    while registry.len() > CAP {
        registry.pop_front();
    }
    health
}

/// DRM FourCC from a four-byte name, little-endian (`b"XR24"`).
const fn fourcc(c: &[u8; 4]) -> u32 {
    (c[0] as u32) | ((c[1] as u32) << 8) | ((c[2] as u32) << 16) | ((c[3] as u32) << 24)
}

pub fn probe() -> anyhow::Result<()> {
    let _importer = EglImporter::new()?;
    let ctx = cuda::context()?;
    tracing::info!(cuda_ctx = ?ctx, "zero-copy probe OK — EGL display + CUDA context initialized");
    let mut worker = client::RemoteImporter::spawn()?;
    let modifiers = worker.supported_modifiers(fourcc(b"XR24")).len();
    tracing::info!(
        modifiers,
        "zero-copy probe OK — worker spawned, handshake + modifier query"
    );
    Ok(())
}

/// BT.709 limited-range RGB→YUV matching the [`egl`] shaders.
/// Y in [16, 235], U/V in [16, 240].
fn bt709_limited(r: u8, g: u8, b: u8) -> (f64, f64, f64) {
    let (r, g, b) = (r as f64 / 255.0, g as f64 / 255.0, b as f64 / 255.0);
    let y = 16.0 + 219.0 * (0.2126 * r + 0.7152 * g + 0.0722 * b);
    let u = 128.0 + 224.0 * (-0.1146 * r - 0.3854 * g + 0.5000 * b);
    let v = 128.0 + 224.0 * (0.5000 * r - 0.4542 * g - 0.0458 * b);
    (y, u, v)
}

/// GPU NV12 convert vs a BT.709 limited-range reference, no display.
/// PASS if max abs error Y ≤ 2, U/V ≤ 3.
pub fn nv12_selftest() -> anyhow::Result<()> {
    use anyhow::bail;

    // Even dims. 16×16 flat blocks so each 2×2 chroma footprint is uniform
    // (exact U/V compare). Remainder is a per-pixel gradient (Y only).
    const W: u32 = 64;
    const H: u32 = 64;
    const BLK: u32 = 16;
    let named: [(&str, u8, u8, u8); 8] = [
        ("red", 255, 0, 0),
        ("green", 0, 255, 0),
        ("blue", 0, 0, 255),
        ("white", 255, 255, 255),
        ("black", 0, 0, 0),
        ("gray128", 128, 128, 128),
        ("yellow", 255, 255, 0),
        ("cyan", 0, 255, 255),
    ];

    let mut rgba = vec![0u8; (W * H * 4) as usize];
    let mut flat = vec![false; (W * H) as usize];
    let grid_cols = W / BLK;
    let pixel_rgb = |x: u32, y: u32| -> (u8, u8, u8, bool) {
        let bx = x / BLK;
        let by = y / BLK;
        let idx = (by * grid_cols + bx) as usize;
        if idx < named.len() {
            let (_, r, g, b) = named[idx];
            (r, g, b, true)
        } else {
            let r = ((x * 4) & 0xff) as u8;
            let g = ((y * 4) & 0xff) as u8;
            let b = (((x + y) * 2) & 0xff) as u8;
            (r, g, b, false)
        }
    };
    for y in 0..H {
        for x in 0..W {
            let (r, g, b, is_flat) = pixel_rgb(x, y);
            let i = ((y * W + x) * 4) as usize;
            rgba[i] = r;
            rgba[i + 1] = g;
            rgba[i + 2] = b;
            rgba[i + 3] = 255;
            flat[(y * W + x) as usize] = is_flat;
        }
    }

    let mut importer = EglImporter::new()?;
    let nv12 = importer.convert_rgba_for_test(&rgba, W, H)?;
    let (uv_ptr, uv_pitch) = nv12
        .uv()
        .ok_or_else(|| anyhow::anyhow!("self-test buffer is not NV12"))?;
    let y_host = cuda::read_plane_to_host(nv12.ptr, nv12.pitch, W as usize, H as usize)?;
    let uv_host = cuda::read_plane_to_host(uv_ptr, uv_pitch, (W as usize / 2) * 2, H as usize / 2)?;

    let mut max_y_err = 0.0f64;
    for y in 0..H {
        for x in 0..W {
            let (r, g, b, _) = pixel_rgb(x, y);
            let (ref_y, _, _) = bt709_limited(r, g, b);
            let got = y_host[(y * W + x) as usize] as f64;
            max_y_err = max_y_err.max((got - ref_y).abs());
        }
    }

    // Chroma is W/2 × H/2, interleaved [U, V]. Compare only uniform 2×2 flats.
    let cw = W / 2;
    let ch = H / 2;
    let mut max_u_err = 0.0f64;
    let mut max_v_err = 0.0f64;
    for cy in 0..ch {
        for cx in 0..cw {
            let (sx, sy) = (cx * 2, cy * 2);
            let all_flat =
                (0..2).all(|dy| (0..2).all(|dx| flat[((sy + dy) * W + (sx + dx)) as usize]));
            if !all_flat {
                continue;
            }
            let (r, g, b, _) = pixel_rgb(sx, sy);
            let (_, ref_u, ref_v) = bt709_limited(r, g, b);
            let base = ((cy * cw + cx) * 2) as usize;
            let got_u = uv_host[base] as f64;
            let got_v = uv_host[base + 1] as f64;
            max_u_err = max_u_err.max((got_u - ref_u).abs());
            max_v_err = max_v_err.max((got_v - ref_v).abs());
        }
    }

    println!("NV12 self-test ({W}x{H}, BT.709 limited range)");
    println!(
        "  {:<8} {:>14} {:>14} {:>14}",
        "color", "Y exp/got", "U exp/got", "V exp/got"
    );
    for (idx, (name, r, g, b)) in named.iter().enumerate() {
        let bx = (idx as u32 % grid_cols) * BLK + BLK / 2;
        let by = (idx as u32 / grid_cols) * BLK + BLK / 2;
        let (ey, eu, ev) = bt709_limited(*r, *g, *b);
        let gy = y_host[(by * W + bx) as usize] as f64;
        let (ccx, ccy) = (bx / 2, by / 2);
        let cbase = ((ccy * cw + ccx) * 2) as usize;
        let gu = uv_host[cbase] as f64;
        let gv = uv_host[cbase + 1] as f64;
        println!(
            "  {:<8} {:>6.1}/{:<6.0} {:>6.1}/{:<6.0} {:>6.1}/{:<6.0}",
            name, ey, gy, eu, gu, ev, gv
        );
    }
    println!(
        "  max abs error:  Y={max_y_err:.2} (≤2)   U={max_u_err:.2} (≤3)   V={max_v_err:.2} (≤3)"
    );

    if max_y_err <= 2.0 && max_u_err <= 3.0 && max_v_err <= 3.0 {
        println!("PASS");
        Ok(())
    } else {
        println!("FAIL");
        bail!("NV12 self-test FAILED (Y={max_y_err:.2} U={max_u_err:.2} V={max_v_err:.2})");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sole oracle for the GPU colour self-test. Pin to external BT.709
    /// limited-range anchors; a typo here fails a correct GPU, or, copied
    /// into the shaders, passes wrong output.
    #[test]
    fn bt709_limited_reference_matches_known_anchors() {
        let close = |a: f64, b: f64| (a - b).abs() < 1e-9;

        let (y, u, v) = bt709_limited(0, 0, 0);
        assert!(
            close(y, 16.0) && close(u, 128.0) && close(v, 128.0),
            "black → ({y}, {u}, {v})"
        );

        let (y, u, v) = bt709_limited(255, 255, 255);
        assert!(
            close(y, 235.0) && close(u, 128.0) && close(v, 128.0),
            "white → ({y}, {u}, {v})"
        );

        // Red saturates V (Kr row sums to +0.5); blue saturates U.
        let (_, _, v) = bt709_limited(255, 0, 0);
        assert!(close(v, 240.0), "red V → {v}");
        let (_, u, _) = bt709_limited(0, 0, 255);
        assert!(close(u, 240.0), "blue U → {u}");

        // Mid-scale so a swapped Kr/Kb pair cannot cancel: BT.709 Y of
        // pure green is 16 + 219·0.7152.
        let (y, _, _) = bt709_limited(0, 255, 0);
        assert!(close(y, 16.0 + 219.0 * 0.7152), "green Y → {y}");
    }

    #[test]
    fn gpu_import_death_latch_is_scoped_to_one_identity() {
        let failing = zero_copy_health(0x1001);
        let healthy = zero_copy_health(0x1002);
        failing.note_gpu_import_death();
        failing.note_gpu_import_ok();
        failing.note_gpu_import_death();
        failing.note_gpu_import_death();
        assert!(!failing.gpu_import_disabled());
        failing.note_gpu_import_death();
        assert!(failing.gpu_import_disabled());
        assert!(!healthy.gpu_import_disabled());
    }

    #[test]
    fn import_failures_latch_and_stay_latched() {
        let l = RawDmabufLatch::new();
        assert!(!l.disabled());
        assert_eq!(l.note_import_failure(), None);
        assert_eq!(l.note_import_failure(), None);
        assert!(!l.disabled(), "must not latch before the streak completes");
        assert_eq!(l.note_import_failure(), Some(3));
        assert!(l.disabled());
        // First crossing only, so the error line cannot repeat per frame.
        assert_eq!(l.note_import_failure(), None);
        // Success resets the streak, not the latch: after CPU fallback there
        // are no dmabuf imports left to succeed.
        l.note_import_ok();
        assert!(l.disabled());
    }

    #[test]
    fn a_success_breaks_the_import_streak() {
        let l = RawDmabufLatch::new();
        l.note_import_failure();
        l.note_import_failure();
        l.note_import_ok();
        assert_eq!(l.note_import_failure(), None, "streak restarted at 1");
        assert_eq!(l.note_import_failure(), None);
        assert!(!l.disabled());
        assert_eq!(l.note_import_failure(), Some(3));
    }

    #[test]
    fn a_negotiation_timeout_is_retried_before_it_latches() {
        let l = RawDmabufLatch::new();
        assert_eq!(l.note_negotiation_timeout(), None, "first one retries");
        assert!(
            !l.disabled(),
            "the next capture build must still be allowed to try dmabuf"
        );
        assert_eq!(l.note_negotiation_timeout(), Some(2));
        assert!(l.disabled());
        assert_eq!(l.note_negotiation_timeout(), None, "reports once");
    }

    #[test]
    fn a_negotiated_capture_credits_the_retry_budget() {
        let l = RawDmabufLatch::new();
        for _ in 0..10 {
            assert_eq!(l.note_negotiation_timeout(), None);
            l.note_negotiation_ok();
        }
        assert!(!l.disabled());
    }

    #[test]
    fn raw_latches_are_sticky_per_identity_and_independent() {
        let failing = zero_copy_health(0x2001);
        let same = zero_copy_health(0x2001);
        let other = zero_copy_health(0x2002);
        assert!(!failing.note_raw_import_failure(0, "one"));
        assert!(!failing.note_raw_import_failure(0, "two"));
        assert!(failing.note_raw_import_failure(0, "three"));
        assert!(same.raw_disabled(), "the same identity keeps its verdict");
        assert!(!other.raw_disabled(), "another identity stays independent");
    }

    #[test]
    fn tiled_import_failure_refuses_tiled_without_spending_three_frames() {
        let health = zero_copy_health(0x2801);
        let other = zero_copy_health(0x2802);
        assert!(health.note_raw_import_failure(7, "tiled reject"));
        assert!(
            !health.raw_disabled(),
            "LINEAR passthrough remains available"
        );
        assert!(health.passthrough_tiled_refused());
        assert!(health.hdr_tiled_refused());
        assert!(!other.passthrough_tiled_refused());
        assert!(!other.hdr_tiled_refused());
    }

    #[test]
    fn planar_refusal_is_sticky_per_identity() {
        let health = zero_copy_health(0x2a01);
        health.refuse_planar();
        assert!(zero_copy_health(0x2a01).planar_refused());
        assert!(!zero_copy_health(0x2a02).planar_refused());
        assert!(!health.raw_disabled(), "packed passthrough stays available");
    }

    #[test]
    fn gpu_negotiation_failure_is_scoped_to_one_identity() {
        let failing = zero_copy_health(0x2901);
        let healthy = zero_copy_health(0x2902);
        failing.note_gpu_negotiation_failed();
        assert!(failing.gpu_negotiation_disabled());
        assert!(!healthy.gpu_negotiation_disabled());
    }

    #[test]
    fn concurrent_capture_success_cannot_clear_another_identitys_failures() {
        let failing = zero_copy_health(0x3001);
        let healthy = zero_copy_health(0x3002);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                for _ in 0..RAW_DMABUF_FAILURE_LATCH {
                    failing.note_raw_import_failure(0, "probe");
                }
            });
            scope.spawn(|| {
                for _ in 0..100 {
                    healthy.note_raw_import_ok();
                }
            });
        });
        assert!(failing.raw_disabled());
        assert!(!healthy.raw_disabled());
    }

    /// Session-open line names the cause: "never offered" vs "failed earlier"
    /// are different bugs.
    #[test]
    fn latch_state_names_the_cause() {
        let l = RawDmabufLatch::new();
        assert_eq!(l.state(), "live");
        l.note_negotiation_timeout();
        l.note_negotiation_timeout();
        assert_eq!(l.state(), "latched: negotiation timeouts");
        let l = RawDmabufLatch::new();
        for _ in 0..RAW_DMABUF_FAILURE_LATCH {
            l.note_import_failure();
        }
        assert_eq!(l.state(), "latched: encoder-import failures");
        for _ in 0..RAW_DMABUF_NEGOTIATION_LATCH {
            l.note_negotiation_timeout();
        }
        assert_eq!(l.state(), "latched: encoder-import + negotiation");
    }
}
