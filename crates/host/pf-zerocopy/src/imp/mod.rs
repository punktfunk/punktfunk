//! Zero-copy capture→encode: the PipeWire dmabuf is imported into CUDA, through its
//! own driver memory object or EGL, and handed to NVENC, so the CPU never copies
//! pixels. VAAPI LINEAR-dmabuf passthrough is the same idea without the hop.
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
pub mod direct;
pub mod drm;
pub mod egl;
pub mod gbm;
// Shared worker rails (SEQPACKET ± `SCM_RIGHTS`, pinned-exe spawn, reaping).
// Message body is generic; `proto` is this worker's vocabulary only.
pub mod ipc;
mod ladder;
pub mod proto;
pub mod rm;
#[cfg(test)]
mod tiled_pattern;
pub mod vkdev;
pub mod vkslot;
pub mod vulkan;
pub mod worker;

pub use cuda::DeviceBuffer;
pub use direct::{DirectImporter, Mapping, Span};
pub use egl::{DmabufPlane, EglImporter};
pub use ladder::{step, Evidence, Ladder, Lane, Refusal};
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

/// `PUNKTFUNK_NVENC_DIRECT=0` keeps a LINEAR raw frame on the worker's copy. Default on: NVENC
/// encodes the producer's buffer in place ([`direct`]).
pub fn nvenc_direct_enabled() -> bool {
    flag_opt("PUNKTFUNK_NVENC_DIRECT").unwrap_or(true)
}

/// `PUNKTFUNK_NVENC_RM_IMPORT=0` keeps the in-place lane on its EGL import. Default on: the
/// lane maps a buffer through the memory object NVIDIA's driver holds behind it ([`rm`]).
pub fn nvenc_rm_import_enabled() -> bool {
    flag_opt("PUNKTFUNK_NVENC_RM_IMPORT").unwrap_or(true)
}

/// Can this box run the fused convert at all? Asks a worker to export the convert timeline,
/// which builds the whole pipeline (modifier import, timeline export, the shader) and so answers
/// the same question the first frame would — before a session is committed to the raw lane.
///
/// A worker's answer is kept: it is a property of the driver, not of a session or its range.
/// `false` keeps capture on the import path from the start instead of failing the first session
/// that reaches the convert. A probe no worker answered is `false` for now and runs again after
/// [`FUSED_RETRY_AFTER`], not on every ask.
pub fn fused_convert_available() -> bool {
    static VERDICT: std::sync::Mutex<FusedVerdict> = std::sync::Mutex::new(FusedVerdict::Unasked);
    fused_convert_in(&VERDICT, std::time::Instant::now(), probe_fused_convert)
}

/// How long an unanswered fused-convert probe stays unasked. A driver still starting is up
/// within seconds; a box that never answers costs one worker spawn per half minute.
const FUSED_RETRY_AFTER: std::time::Duration = std::time::Duration::from_secs(30);

enum FusedVerdict {
    Unasked,
    Missed(std::time::Instant),
    Answered(bool),
}

/// The kept verdict, else what `probe` finds unless one missed inside [`FUSED_RETRY_AFTER`] of
/// `now`. The probe runs under the lock, so concurrent asks wait for one probe.
fn fused_convert_in(
    cell: &std::sync::Mutex<FusedVerdict>,
    now: std::time::Instant,
    probe: impl FnOnce() -> Option<bool>,
) -> bool {
    let mut verdict = cell.lock().unwrap_or_else(|e| e.into_inner());
    match *verdict {
        FusedVerdict::Answered(a) => return a,
        FusedVerdict::Missed(at) if now.saturating_duration_since(at) < FUSED_RETRY_AFTER => {
            return false;
        }
        _ => {}
    }
    match probe() {
        Some(a) => {
            *verdict = FusedVerdict::Answered(a);
            a
        }
        None => {
            *verdict = FusedVerdict::Missed(now);
            false
        }
    }
}

/// `Some(verdict)` once a worker answered; `None` when none did (it did not start, or died
/// mid-ask).
fn probe_fused_convert() -> Option<bool> {
    if !nvenc_raw_enabled() {
        return Some(false);
    }
    let mut importer = match Importer::new_for_capture(true) {
        Ok(i) => i,
        Err(e) => {
            tracing::info!(
                error = %format!("{e:#}"),
                "fused convert probe has no worker to ask — asked again later"
            );
            return None;
        }
    };
    match importer.convert_timeline() {
        Ok(_) => Some(true),
        Err(e) if importer.dead() => {
            tracing::info!(
                error = %format!("{e:#}"),
                "fused convert probe lost its worker — asked again later"
            );
            None
        }
        Err(e) => {
            tracing::info!(
                error = %format!("{e:#}"),
                "fused convert unavailable on this box — capture keeps the import path"
            );
            Some(false)
        }
    }
}

/// Tiled modifiers the fused convert can sample for packed-RGB `fourcc`; LINEAR excluded, driver
/// order. The raw lane offers only these: a producer that fixates another (NVIDIA's compressed
/// block-linear) would be refused on the first frame.
///
/// Cached per fourcc: the answer is a property of the driver, not of a session.
pub fn fused_convert_modifiers(fourcc: u32) -> Vec<u64> {
    static CACHE: std::sync::Mutex<Vec<(u32, Vec<u64>)>> = std::sync::Mutex::new(Vec::new());
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((_, m)) = cache.iter().find(|(f, _)| *f == fourcc) {
        return m.clone();
    }
    let mut mods = drm::vk_format(fourcc).map_or_else(Vec::new, vkdev::sampled_import_modifiers);
    mods.retain(|&m| m != 0);
    mods.dedup();
    cache.push((fourcc, mods.clone()));
    mods
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
    /// An importer whose every YUV convert (NV12, YUV444, the fused pass) writes the session's
    /// `full_range`, or studio. `Err` means no GPU import — callers fall back to CPU.
    pub fn new_for_capture(full_range: bool) -> anyhow::Result<Importer> {
        if flag("PUNKTFUNK_ZEROCOPY_INPROC") {
            tracing::warn!(
                "PUNKTFUNK_ZEROCOPY_INPROC=1 — GPU import runs IN-PROCESS; a driver fault on a \
                 dying compositor's dmabuf can take the whole host down (debug/A-B use only)"
            );
            return Ok(Importer::InProc(Box::new(EglImporter::new(full_range)?)));
        }
        Ok(Importer::Remote(client::RemoteImporter::spawn(full_range)?))
    }

    /// In-process: its GL context is current only on the thread that built it, so every
    /// import stays on that thread.
    pub fn in_process(&self) -> bool {
        matches!(self, Importer::InProc(_))
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

/// Failure memory for one capture identity: one [`Ladder`] behind a lock. Clones share only that
/// source's verdicts; concurrent sessions on another node cannot clear or poison them.
#[derive(Clone, Debug)]
pub struct ZeroCopyHealth(std::sync::Arc<std::sync::Mutex<Ladder>>);

impl ZeroCopyHealth {
    /// This identity's ladder as of now.
    pub fn ladder(&self) -> Ladder {
        *self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Apply `evidence` in order; the ladder before and after.
    pub fn note(&self, evidence: &[Evidence]) -> (Ladder, Ladder) {
        let mut ladder = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let before = *ladder;
        *ladder = evidence.iter().fold(before, |l, e| step(l, *e));
        (before, *ladder)
    }

    pub fn raw_disabled(&self) -> bool {
        self.ladder().off(Lane::Raw)
    }

    /// Record an encoder import failure. Tiled refuses immediately; LINEAR keeps the
    /// three-failure budget. `true` asks the owning capture to rebuild on its safe offer.
    pub fn note_raw_import_failure(&self, modifier: u64, reason: &str) -> bool {
        if modifier != 0 {
            let tiled = [Refusal::Tiled, Refusal::HdrTiled].map(Evidence::Refused);
            let (before, _) = self.note(&tiled);
            if !before.refused(Refusal::Tiled) {
                tracing::error!(
                    reason,
                    "tiled raw-dmabuf import rejected for this capture — rebuilding on LINEAR"
                );
            }
            return true;
        }
        let (before, after) = self.note(&[Evidence::ImportFailed(Lane::Raw)]);
        let latched = !before.import_off(Lane::Raw) && after.import_off(Lane::Raw);
        if latched {
            tracing::error!(
                reason,
                "LINEAR raw-dmabuf import disabled for this capture after repeated failures"
            );
        }
        latched
    }

    /// Per frame on the encoder: clears the failure streak, never a spent lane.
    pub fn note_raw_import_ok(&self) {
        self.note(&[Evidence::Imported(Lane::Raw)]);
    }

    /// A full first-frame wait on `lane`'s offer ended with no format.
    pub fn note_offer_timed_out(&self, lane: Lane) {
        let (before, after) = self.note(&[Evidence::OfferTimedOut(lane)]);
        let latched = !before.offer_off(lane) && after.offer_off(lane);
        match (lane, latched) {
            (Lane::Raw, true) => tracing::warn!(
                streak = ladder::RAW_OFFER_BUDGET,
                "zero-copy raw-dmabuf passthrough disabled for this capture after repeated negotiation timeouts"
            ),
            (Lane::Raw, false) => tracing::warn!(
                "the compositor did not accept the raw-dmabuf offer — retrying once"
            ),
            (Lane::Gpu, true) => tracing::warn!(
                "zero-copy EGL-to-CUDA dmabuf offer disabled for this capture after a negotiation timeout"
            ),
            (Lane::Gpu, false) => {}
        }
    }

    pub fn raw_state(&self) -> &'static str {
        self.ladder().raw_state()
    }

    pub fn note_gpu_import_lost(&self) {
        let (before, after) = self.note(&[Evidence::ImportFailed(Lane::Gpu)]);
        if !before.import_off(Lane::Gpu) && after.import_off(Lane::Gpu) {
            tracing::error!(
                streak = ladder::GPU_IMPORT_BUDGET,
                "zero-copy GPU import disabled for this capture after repeated lost imports"
            );
        }
    }

    pub fn passthrough_tiled_refused(&self) -> bool {
        self.ladder().refused(Refusal::Tiled)
    }

    pub fn hdr_tiled_refused(&self) -> bool {
        self.ladder().refused(Refusal::HdrTiled)
    }

    /// Withdraw an offer for this identity. `true` on the first refusal.
    pub fn refuse(&self, refusal: Refusal) -> bool {
        let (before, _) = self.note(&[Evidence::Refused(refusal)]);
        !before.refused(refusal)
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
    let health = ZeroCopyHealth(std::sync::Arc::default());
    registry.push_back((identity, health.clone()));
    while registry.len() > CAP {
        registry.pop_front();
    }
    health
}

pub fn probe() -> anyhow::Result<()> {
    let _importer = EglImporter::new(true)?;
    let ctx = cuda::context()?;
    tracing::info!(cuda_ctx = ?ctx, "zero-copy probe OK — EGL display + CUDA context initialized");
    let mut worker = client::RemoteImporter::spawn(true)?;
    let modifiers = worker.supported_modifiers(drm::XR24).len();
    tracing::info!(
        modifiers,
        "zero-copy probe OK — worker spawned, handshake + modifier query"
    );
    Ok(())
}

/// BT.709 RGB→YUV matching the [`egl`] shaders. Full range: Y in [0, 255], U/V in
/// [0.5, 255.5] before the shader's clamp. Studio: Y in [16, 235], U/V in [16, 240].
fn bt709(r: u8, g: u8, b: u8, full_range: bool) -> (f64, f64, f64) {
    let (r, g, b) = (r as f64 / 255.0, g as f64 / 255.0, b as f64 / 255.0);
    let (y_off, y_scale, c_scale) = if full_range {
        (0.0, 255.0, 255.0)
    } else {
        (16.0, 219.0, 224.0)
    };
    let y = y_off + y_scale * (0.2126 * r + 0.7152 * g + 0.0722 * b);
    let u = 128.0 + c_scale * (-0.1146 * r - 0.3854 * g + 0.5000 * b);
    let v = 128.0 + c_scale * (0.5000 * r - 0.4542 * g - 0.0458 * b);
    (y, u, v)
}

/// GPU NV12 convert vs the BT.709 reference at full and at studio range, no display.
/// PASS if max abs error Y ≤ 2, U/V ≤ 3 at both.
pub fn nv12_selftest() -> anyhow::Result<()> {
    let full = nv12_selftest_at(true);
    nv12_selftest_at(false).and(full)
}

fn nv12_selftest_at(full_range: bool) -> anyhow::Result<()> {
    use anyhow::bail;
    let range = if full_range { "full" } else { "studio" };

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

    let mut importer = EglImporter::new(full_range)?;
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
            let (ref_y, _, _) = bt709(r, g, b, full_range);
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
            let (_, ref_u, ref_v) = bt709(r, g, b, full_range);
            let base = ((cy * cw + cx) * 2) as usize;
            let got_u = uv_host[base] as f64;
            let got_v = uv_host[base + 1] as f64;
            max_u_err = max_u_err.max((got_u - ref_u).abs());
            max_v_err = max_v_err.max((got_v - ref_v).abs());
        }
    }

    println!("NV12 self-test ({W}x{H}, BT.709 {range} range)");
    println!(
        "  {:<8} {:>14} {:>14} {:>14}",
        "color", "Y exp/got", "U exp/got", "V exp/got"
    );
    for (idx, (name, r, g, b)) in named.iter().enumerate() {
        let bx = (idx as u32 % grid_cols) * BLK + BLK / 2;
        let by = (idx as u32 / grid_cols) * BLK + BLK / 2;
        let (ey, eu, ev) = bt709(*r, *g, *b, full_range);
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
        bail!(
            "NV12 {range}-range self-test out of bounds: Y={max_y_err:.2} U={max_u_err:.2} \
             V={max_v_err:.2}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sole oracle for the GPU colour self-test. Pin to external BT.709
    /// anchors at both ranges; a typo here fails a correct GPU, or, copied
    /// into the shaders, passes wrong output.
    #[test]
    fn bt709_reference_matches_known_anchors() {
        let close = |a: f64, b: f64| (a - b).abs() < 1e-9;
        // (full, black Y, white Y, saturated U/V, luma span)
        for (full, black, white, sat, span) in [
            (true, 0.0, 255.0, 255.5, 255.0),
            (false, 16.0, 235.0, 240.0, 219.0),
        ] {
            let (y, u, v) = bt709(0, 0, 0, full);
            assert!(
                close(y, black) && close(u, 128.0) && close(v, 128.0),
                "{full}: black → ({y}, {u}, {v})"
            );

            let (y, u, v) = bt709(255, 255, 255, full);
            assert!(
                close(y, white) && close(u, 128.0) && close(v, 128.0),
                "{full}: white → ({y}, {u}, {v})"
            );

            // Red saturates V (Kr row sums to +0.5); blue saturates U. Full: 255.5 before the
            // clamp.
            let (_, _, v) = bt709(255, 0, 0, full);
            assert!(close(v, sat), "{full}: red V → {v}");
            let (_, u, _) = bt709(0, 0, 255, full);
            assert!(close(u, sat), "{full}: blue U → {u}");

            // Mid-scale so a swapped Kr/Kb pair cannot cancel: BT.709 Y of
            // pure green is black + span·0.7152.
            let (y, _, _) = bt709(0, 255, 0, full);
            assert!(close(y, black + span * 0.7152), "{full}: green Y → {y}");
        }
    }

    #[test]
    fn gpu_lane_latches_are_scoped_to_one_identity() {
        let failing = zero_copy_health(0x1001);
        let healthy = zero_copy_health(0x1002);
        failing.note_gpu_import_lost();
        failing.note(&[Evidence::Imported(Lane::Gpu)]);
        failing.note_gpu_import_lost();
        failing.note_gpu_import_lost();
        assert!(!failing.ladder().off(Lane::Gpu));
        failing.note_gpu_import_lost();
        assert!(failing.ladder().off(Lane::Gpu));
        assert!(!healthy.ladder().off(Lane::Gpu));
        let refused = zero_copy_health(0x1003);
        refused.note_offer_timed_out(Lane::Gpu);
        assert!(
            refused.ladder().off(Lane::Gpu),
            "one GPU offer timeout is enough"
        );
        assert!(!refused.raw_disabled(), "the raw lane has its own offer");
    }

    /// `true` once, at the crossing, so the rebuild and its error line cannot repeat per frame.
    #[test]
    fn raw_latches_are_sticky_per_identity_and_independent() {
        let failing = zero_copy_health(0x2001);
        let same = zero_copy_health(0x2001);
        let other = zero_copy_health(0x2002);
        assert!(!failing.note_raw_import_failure(0, "one"));
        assert!(!failing.note_raw_import_failure(0, "two"));
        assert!(failing.note_raw_import_failure(0, "three"));
        assert!(!failing.note_raw_import_failure(0, "four"));
        failing.note_raw_import_ok();
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
        assert!(health.refuse(Refusal::Planar));
        assert!(!health.refuse(Refusal::Planar), "once");
        assert!(zero_copy_health(0x2a01).ladder().refused(Refusal::Planar));
        assert!(!zero_copy_health(0x2a02).ladder().refused(Refusal::Planar));
        assert!(!health.raw_disabled(), "packed passthrough stays available");
    }

    #[test]
    fn concurrent_capture_success_cannot_clear_another_identitys_failures() {
        let failing = zero_copy_health(0x3001);
        let healthy = zero_copy_health(0x3002);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                for _ in 0..ladder::RAW_IMPORT_BUDGET {
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

    /// A probe no worker answered is `false` until the hold-off passes, then asks again; an
    /// answer, `false` included, is never asked again.
    #[test]
    fn an_unanswered_fused_probe_is_asked_again_after_the_hold_off() {
        use std::time::{Duration, Instant};
        fn kept() -> Option<bool> {
            panic!("an answered cell is not asked again")
        }
        let cell = std::sync::Mutex::new(FusedVerdict::Unasked);
        let t0 = Instant::now();
        let mut asks = 0;
        assert!(!fused_convert_in(&cell, t0, || {
            asks += 1;
            None
        }));
        let inside = t0 + FUSED_RETRY_AFTER - Duration::from_secs(1);
        assert!(!fused_convert_in(&cell, inside, || {
            asks += 1;
            Some(true)
        }));
        assert_eq!(asks, 1, "no second probe inside the hold-off");
        assert!(fused_convert_in(&cell, t0 + FUSED_RETRY_AFTER, || {
            asks += 1;
            Some(true)
        }));
        assert_eq!(asks, 2);
        assert!(fused_convert_in(&cell, t0 + FUSED_RETRY_AFTER * 9, kept));

        let refused = std::sync::Mutex::new(FusedVerdict::Unasked);
        assert!(!fused_convert_in(&refused, t0, || Some(false)));
        assert!(!fused_convert_in(
            &refused,
            t0 + FUSED_RETRY_AFTER * 9,
            kept
        ));
    }
}
