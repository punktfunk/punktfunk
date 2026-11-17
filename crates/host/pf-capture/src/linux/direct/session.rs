//! A direct capture session's setup on the capture thread: connect, bind the globals, read the
//! output's colour, start the session and build the pool. The two HDR probes and the bounded
//! waits both use live here too; the probes run on their callers' threads.

use super::pool::{render_node_for, FreeList, GbmPool};
use super::protocol::{dmabuf, icc, new_state, CmOutput, ColorManager, OutputColor, State};
use super::Opened;
use crate::linux::StreamShared;
use anyhow::{anyhow, bail, Context, Result};
use pf_frame::PixelFormat;
use pf_zerocopy::drm;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use wayland_client::protocol::{wl_buffer, wl_output};
use wayland_client::{ConnectError, Connection, EventQueue, QueueHandle};

/// Buffers in the pool. Two are the working pair (one being filled, one in flight); the
/// rest absorb an encoder that holds a frame past the next capture. Above this the extra
/// memory buys nothing — the compositor only ever fills one at a time.
const POOL: usize = 4;

/// Dispatch until `done` holds or `timeout` passes. Bounded: a `blocking_dispatch` never
/// returns for a compositor that answers nothing, and a probe runs this on its caller's
/// thread.
fn pump_until(
    conn: &Connection,
    queue: &mut EventQueue<State>,
    st: &mut State,
    wake: &OwnedFd,
    timeout: Duration,
    done: impl Fn(&State) -> bool,
) -> Result<bool> {
    let until = Instant::now() + timeout;
    while !done(st) && !st.stopped && Instant::now() < until {
        conn.flush().context("wayland flush")?;
        wait_readable(conn, wake, Duration::from_millis(50))?;
        queue.dispatch_pending(st).context("wayland dispatch")?;
    }
    Ok(done(st))
}

/// `queue.roundtrip` bounded by `timeout`: a probe runs on its caller's thread. `false`
/// when the compositor did not answer in time.
fn roundtrip_within(
    conn: &Connection,
    queue: &mut EventQueue<State>,
    st: &mut State,
    wake: &OwnedFd,
    timeout: Duration,
) -> Result<bool> {
    st.synced = false;
    let _callback = conn.display().sync(&queue.handle(), ());
    pump_until(conn, queue, st, wake, timeout, |s| s.synced)
}

/// The output's current image description, read through `get_information`.
///
/// Returns the `wp_color_management_output_v1` to keep for its `image_description_changed`
/// event, and the description. `Ok(None)` when the compositor cannot describe the output
/// (gone, or an older interface); the caller treats that as SDR.
fn fetch_output_color(
    conn: &Connection,
    queue: &mut EventQueue<State>,
    st: &mut State,
    qh: &QueueHandle<State>,
    mgr: &ColorManager,
    output: &wl_output::WlOutput,
    wake: &OwnedFd,
) -> Result<Option<(CmOutput, OutputColor)>> {
    let cm_out = mgr.get_output(output, qh, ());
    let desc = cm_out.get_image_description(qh, ());
    st.desc_ready = false;
    st.desc_failed = None;
    // `ready` or `failed` lands at once; `get_information` before `ready` is a protocol error.
    if !pump_until(conn, queue, st, wake, Duration::from_secs(2), |s| {
        s.desc_ready || s.desc_failed.is_some()
    })? {
        desc.destroy();
        cm_out.destroy();
        bail!("compositor did not answer the output's image description");
    }
    if let Some(msg) = st.desc_failed.take() {
        desc.destroy();
        cm_out.destroy();
        tracing::debug!(reason = %msg, "output has no colour description — taking it as SDR");
        return Ok(None);
    }
    st.color_pending = OutputColor::default();
    st.color_done = false;
    // `done` is a destructor event: the info object is gone once it lands.
    let _info = desc.get_information(qh, ());
    let done = pump_until(conn, queue, st, wake, Duration::from_secs(2), |s| {
        s.color_done
    })?;
    desc.destroy();
    if !done {
        cm_out.destroy();
        bail!("compositor did not finish the output's colour information");
    }
    Ok(Some((cm_out, st.color_pending)))
}

/// A connection to the live compositor: the stored session's socket, or the environment's
/// while nothing is stored.
fn wayland_connect() -> Result<Connection, ConnectError> {
    match pf_host_config::session::wayland_stream() {
        Some(stream) => Connection::from_socket(stream.map_err(|_| ConnectError::NoCompositor)?),
        None => Connection::connect_to_env(),
    }
}

/// How long a probe waits on each compositor round trip. The handshake and `/serverinfo`
/// call the probes, and a stopped compositor must not hold them.
const PROBE_ROUNDTRIP: Duration = Duration::from_secs(2);

/// Whether `output_name` is lit in HDR (BT.2020 PQ), by its `wp_color_management_v1`
/// description. `None`: no such output, a compositor without colour management, or one
/// that does not answer.
pub(crate) fn output_is_hdr10(output_name: &str) -> Option<bool> {
    let (wake_r, _wake_w) = pipe().ok()?;
    let conn = wayland_connect().ok()?;
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    let _registry = conn.display().get_registry(&qh, ());
    let mut st = new_state().ok()?;
    // Globals, then the outputs' names.
    for _ in 0..2 {
        if !roundtrip_within(&conn, &mut queue, &mut st, &wake_r, PROBE_ROUNDTRIP).ok()? {
            return None;
        }
    }
    let mgr = st.color_mgr.clone()?;
    let output = st.output_named(output_name)?.clone();
    let (cm_out, color) =
        fetch_output_color(&conn, &mut queue, &mut st, &qh, &mgr, &output, &wake_r).ok()??;
    cm_out.destroy();
    let _ = conn.flush();
    Some(color.is_hdr10())
}

/// Whether the compositor advertises every global an HDR capture binds in [`open`]. No
/// `wp_color_manager_v1` is the usual miss: Hyprland drops it under `render:cm_enabled = false`.
/// A compositor that does not answer reads as `false`.
pub(crate) fn hdr_capture_globals() -> bool {
    let probe = || {
        let (wake_r, _wake_w) = pipe().ok()?;
        let conn = wayland_connect().ok()?;
        let mut queue = conn.new_event_queue();
        let qh = queue.handle();
        let _registry = conn.display().get_registry(&qh, ());
        let mut st = new_state().ok()?;
        if !roundtrip_within(&conn, &mut queue, &mut st, &wake_r, PROBE_ROUNDTRIP).ok()? {
            return None;
        }
        let (capture, colour) = (
            st.source_mgr.is_some() && st.capture_mgr.is_some() && st.linux_dmabuf.is_some(),
            st.color_mgr.is_some(),
        );
        tracing::debug!(capture, colour, "direct HDR capture globals");
        Some(capture && colour)
    };
    probe().unwrap_or(false)
}

/// Pick the format to allocate, and the modifiers to allocate it with.
///
/// Preference is the encoder's, not ours: `want` is what the consumer imports today.
/// Each fourcc needs an exact consumer list; missing or empty rejects that format.
fn choose_format(
    offered: &[(u32, Vec<u64>)],
    want: &[u32],
    importable: &[(u32, Vec<u64>)],
) -> Option<(u32, Vec<u64>)> {
    for w in want {
        let Some((fourcc, mods)) = offered.iter().find(|(f, _)| f == w) else {
            continue;
        };
        let Some((_, accepted)) = importable.iter().find(|(f, _)| f == w) else {
            continue;
        };
        let mut usable: Vec<u64> = mods
            .iter()
            .copied()
            .filter(|m| accepted.contains(m))
            .collect();
        usable.sort_by_key(|m| (*m != 0, *m));
        usable.dedup();
        if !usable.is_empty() {
            return Some((*fourcc, usable));
        }
    }
    None
}

/// Does this session's encoder need the EGL/CUDA importer?
///
/// libva and PyroWave's Vulkan import a dmabuf themselves. NVENC needs it for the modifier
/// list, and takes CUDA from it when its raw lane is off. Same split the PipeWire path makes
/// through `ZeroCopyPolicy`.
fn needs_cuda_import(policy: &crate::ZeroCopyPolicy) -> bool {
    policy.backend_is_gpu && !policy.backend_is_vaapi && !policy.pyrowave_session
}

/// The pixel format of a buffer this capture allocates: packed RGB only, never the
/// two-plane NV12/P010 a PipeWire producer may offer. The 10-bit pair is the HDR capture,
/// PQ by the output's description.
fn fourcc_to_pixel(fourcc: u32) -> Option<PixelFormat> {
    PixelFormat::from_drm_fourcc(fourcc)
        .filter(|f| !matches!(f, PixelFormat::Nv12 | PixelFormat::P010))
}

/// Fourccs to allocate, by preference. SDR is the packed-RGB pair every encoder ingests.
/// HDR is the packed 10-bit pair, `XB30` first: NVIDIA has no linear `A2R10G10B10`, and
/// Hyprland hands out `XBGR2101010` for a 10-bit head anyway.
fn wanted_fourccs(hdr: bool) -> [u32; 2] {
    if hdr {
        [drm::XB30, drm::XR30]
    } else {
        [drm::XR24, drm::AR24]
    }
}

/// Bind the globals, read the output's colour, start the session and build the pool, up to
/// each buffer wrapped as a `wl_buffer`. Every failure keeps the portal path.
///
/// `Streaming` and the fixated format are stored before this returns: the opener must not
/// see a capturer that is not streaming yet.
pub(super) fn open(
    output_name: &str,
    policy: &crate::ZeroCopyPolicy,
    want_hdr: bool,
    shared: &StreamShared,
) -> Result<Opened> {
    // One pipe for both waits below: the bounded constraints wait, and the frame loop's
    // "a buffer came back" wakeup.
    let (wake_r, wake_w) = pipe().context("wakeup pipe")?;
    let conn = wayland_connect().context("wayland connect")?;
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    let _registry = conn.display().get_registry(&qh, ());
    let mut st = new_state()?;
    queue.roundtrip(&mut st).context("registry roundtrip")?;
    queue.roundtrip(&mut st).context("output-name roundtrip")?;

    let source_mgr = st
        .source_mgr
        .clone()
        .ok_or_else(|| anyhow!("compositor has no ext_output_image_capture_source_manager_v1"))?;
    let capture_mgr = st
        .capture_mgr
        .clone()
        .ok_or_else(|| anyhow!("compositor has no ext_image_copy_capture_manager_v1"))?;
    let linux_dmabuf = st
        .linux_dmabuf
        .clone()
        .ok_or_else(|| anyhow!("compositor has no zwp_linux_dmabuf_v1 (v3+)"))?;
    let output = st
        .output_named(output_name)
        .ok_or_else(|| {
            anyhow!(
                "no wl_output named {output_name} (have: {:?})",
                st.outputs
                    .iter()
                    .filter_map(|(_, n)| n.clone())
                    .collect::<Vec<_>>()
            )
        })?
        .clone();

    // The output's colour comes first. An HDR session on a head lit SDR captures SDR, with no
    // `hdr_meta`, so its encoder and the client follow; the `wp_color_management_output_v1`
    // stays bound to see the head re-lit.
    let color = match st.color_mgr.clone() {
        Some(mgr) => fetch_output_color(&conn, &mut queue, &mut st, &qh, &mgr, &output, &wake_r)?,
        None => None,
    };
    let _cm_out = color.as_ref().map(|(o, _)| o.clone());
    let color = color.map(|(_, c)| c);
    let hdr = match (want_hdr, color) {
        (false, _) => false,
        (true, Some(c)) if c.is_hdr10() => true,
        (true, Some(c)) => {
            tracing::info!(
                output = output_name,
                primaries = c.primaries,
                transfer = c.tf,
                "output is lit SDR under an HDR session — capturing SDR until it is re-lit"
            );
            false
        }
        (true, None) => bail!(
            "compositor describes no colour for output {output_name} (no \
             wp_color_management_v1) — HDR capture needs it"
        ),
    };

    let source = source_mgr.create_source(&output, &qh, ());
    // `paint_cursors`: nothing on this path feeds the host's cursor plane (no
    // `CursorSlot`), so the compositor draws the pointer into the frames.
    let session = capture_mgr.create_session(
        &source,
        icc::ext_image_copy_capture_manager_v1::Options::PaintCursors,
        &qh,
        (),
    );
    // Constraints arrive as a batch ending in `done`.
    if !pump_until(
        &conn,
        &mut queue,
        &mut st,
        &wake_r,
        Duration::from_secs(3),
        |s| s.constraints_done,
    )? {
        bail!("capture session sent no buffer constraints");
    }

    // NVENC's raw lane converts the held dmabuf into its slot in one pass, as libva and
    // PyroWave import it themselves. Once this identity's ladder closes it, NVENC takes CUDA
    // from the EGL/CUDA import instead. The importer is built before the pool either way: it
    // names the allocatable modifiers. One importer, one worker process.
    let raw_lane = crate::linux::raw_lane_ok(policy) && shared.health.ladder().nvenc_raw_ok();
    let mut importer = if needs_cuda_import(policy) {
        if shared.health.ladder().off(pf_zerocopy::Lane::Gpu) {
            bail!("this output's GPU import is off after repeated failures");
        }
        // Direct capture imports packed RGB only, so no convert of its writes a range.
        let importer = pf_zerocopy::Importer::new_for_capture(true).map_err(|e| {
            anyhow!("this session's encoder needs the GPU importer, which did not start: {e:#}")
        })?;
        Some(importer)
    } else {
        None
    };
    // Only direct-SDK NVENC reads packed 10-bit PQ off a CUDA import; any other arm would
    // encode the words as garbage.
    if hdr && importer.is_some() && !raw_lane && !policy.hdr_cuda_ok {
        bail!("this session's encoder takes no 10-bit PQ through the GPU importer");
    }
    let (pool, fourcc, format, (w, h)) = build_pool(&st, importer.as_mut(), policy, hdr, raw_lane)?;

    let mut buffers: Vec<wl_buffer::WlBuffer> = Vec::with_capacity(pool.bos.len());
    let mut all_params = Vec::with_capacity(pool.bos.len());
    for bo in &pool.bos {
        let params = linux_dmabuf.create_params(&qh, ());
        // An implicit layout (`INVALID`) goes out as-is: the compositor rejects it rather
        // than this side guessing one.
        let m = bo.modifier;
        params.add(
            bo.fd.as_fd(),
            0,
            bo.offset,
            bo.stride,
            (m >> 32) as u32,
            (m & 0xffff_ffff) as u32,
        );
        buffers.push(params.create_immed(
            w as i32,
            h as i32,
            fourcc,
            dmabuf::zwp_linux_buffer_params_v1::Flags::empty(),
            &qh,
            (),
        ));
        all_params.push(params);
    }
    // A refused import comes back later: as a fatal protocol error, or `failed` on the
    // params. Both must land here, before the status below, while the caller can still fall
    // back to the portal.
    queue
        .roundtrip(&mut st)
        .context("import the capture dmabufs")?;
    for params in all_params {
        params.destroy();
    }
    if st.import_failed {
        bail!("compositor refused the capture dmabufs");
    }

    let free = Arc::new(FreeList {
        free: Mutex::new((0..pool.bos.len()).collect()),
        wake_w,
    });

    shared.report(crate::linux::StreamStatus::Streaming { driving: false });
    shared.fixate(crate::linux::Fixated { hdr, size: (w, h) });
    let hdr_meta = color.filter(|_| hdr).map(|c| c.hdr_meta());
    tracing::info!(
        output = output_name,
        w,
        h,
        fourcc = format_args!("{:#010x}", fourcc),
        modifier = pool.bos.first().map(|b| b.modifier).unwrap_or(0),
        pool = pool.bos.len(),
        raw_lane,
        hdr,
        "direct wayland capture: the compositor fills our dmabufs, no portal in the path"
    );
    Ok(Opened {
        free,
        buffers,
        pool,
        importer,
        raw_lane,
        fourcc,
        format,
        size: (w, h),
        hdr_meta,
        session,
        source,
        _cm_out,
        st,
        _registry,
        qh,
        queue,
        conn,
        wake_r,
    })
}

/// Intersect each constrained fourcc with its consumer list, then allocate that pool.
/// `hdr` asks the packed 10-bit pair instead of 8-bit RGB; `raw_lane` narrows the importer's
/// list to what the fused convert samples.
fn build_pool(
    st: &State,
    importer: Option<&mut pf_zerocopy::Importer>,
    policy: &crate::ZeroCopyPolicy,
    hdr: bool,
    raw_lane: bool,
) -> Result<(GbmPool, u32, PixelFormat, (u32, u32))> {
    let (w, h) = st
        .size
        .ok_or_else(|| anyhow!("session sent no buffer size"))?;
    let dev = st
        .dmabuf_dev
        .ok_or_else(|| anyhow!("session offered no dmabuf device (shm-only capture)"))?;
    // Every format gets its own consumer-proved list. LINEAR is always supported by
    // the CUDA Vulkan bridge and remains the safe fallback for direct encoders.
    let want = wanted_fourccs(hdr);
    let mut importer = importer;
    let importable: Vec<(u32, Vec<u64>)> = want
        .iter()
        .copied()
        .map(|fourcc| {
            let mut modifiers = if let Some(i) = importer.as_deref_mut() {
                let egl = i.supported_modifiers(fourcc);
                if raw_lane {
                    let fused = pf_zerocopy::fused_convert_modifiers(fourcc);
                    crate::linux::raw_lane_modifiers(&egl, &fused)
                } else {
                    egl
                }
            } else {
                policy
                    .encoder_modifiers
                    .iter()
                    .find(|(f, _)| *f == fourcc)
                    .map(|(_, m)| m.clone())
                    .unwrap_or_default()
            };
            modifiers.retain(|m| *m != 0);
            modifiers.dedup();
            modifiers.push(0);
            (fourcc, modifiers)
        })
        .collect();
    let (fourcc, mods) =
        choose_format(&st.dmabuf_formats, &want, &importable).ok_or_else(|| {
            anyhow!(
                "no usable dmabuf format: compositor offered {:?}, consumer takes {:?}",
                st.dmabuf_formats
                    .iter()
                    .map(|(f, m)| (format!("{:#010x}", f), m.len()))
                    .collect::<Vec<_>>(),
                want.iter()
                    .map(|f| format!("{f:#010x}"))
                    .collect::<Vec<_>>()
            )
        })?;
    let format = fourcc_to_pixel(fourcc)
        .ok_or_else(|| anyhow!("negotiated fourcc {fourcc:#010x} has no pixel format"))?;
    let node = render_node_for(dev)?;
    let pool = GbmPool::new(node, w, h, fourcc, &mods, POOL)?;
    Ok((pool, fourcc, format, (w, h)))
}

fn pipe() -> Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0i32; 2];
    // SAFETY: `pipe2` writes exactly two ints into the live local array and returns 0 on
    // success (checked). CLOEXEC keeps the fds out of any child.
    let rc = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) };
    if rc != 0 {
        bail!("pipe2 failed: {}", std::io::Error::last_os_error());
    }
    // SAFETY: both fds come from the successful `pipe2` above and are owned solely here.
    unsafe {
        use std::os::fd::FromRawFd;
        Ok((OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])))
    }
}

pub(super) fn drain(fd: &OwnedFd) {
    let mut buf = [0u8; 64];
    // SAFETY: a non-blocking read into a live local buffer; a short read or EAGAIN is the
    // expected end and needs no handling.
    while unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) } > 0 {}
}

/// Block until the compositor has events or a buffer came back, whichever first.
///
/// `prepare_read` before the poll is what makes this race-free: events queued between the
/// dispatch and the poll are read here rather than waited on.
pub(super) fn wait_readable(
    conn: &Connection,
    wake: &OwnedFd,
    timeout: std::time::Duration,
) -> Result<()> {
    let guard = conn.prepare_read();
    let mut fds = [
        libc::pollfd {
            fd: conn.as_fd().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: wake.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    // SAFETY: `poll` reads and writes exactly the two live `pollfd`s in this local array.
    let rc = unsafe { libc::poll(fds.as_mut_ptr(), 2, timeout.as_millis() as i32) };
    // `None` means events were already pending, and `dispatch_pending` takes those.
    if let Some(g) = guard {
        if rc > 0 && fds[0].revents != 0 {
            let _ = g.read();
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{choose_format, fourcc_to_pixel, wanted_fourccs};
    use pf_frame::PixelFormat;
    use pf_zerocopy::drm::{AR24, NV12, P010, XB30, XR24, XR30};

    #[test]
    fn hdr_asks_the_packed_ten_bit_pair_with_xb30_first() {
        assert_eq!(wanted_fourccs(true), [XB30, XR30]);
        assert_eq!(wanted_fourccs(false), [XR24, AR24]);
        assert_eq!(fourcc_to_pixel(XB30), Some(PixelFormat::X2Bgr10));
        assert_eq!(fourcc_to_pixel(XR30), Some(PixelFormat::X2Rgb10));
    }

    #[test]
    fn the_wanted_format_wins_over_the_order_the_compositor_offered() {
        let offered = vec![(AR24, vec![1, 2]), (XR24, vec![5])];
        let importable = vec![(XR24, vec![5]), (AR24, vec![1, 2])];
        let (f, m) = choose_format(&offered, &[XR24, AR24], &importable).unwrap();
        assert_eq!(
            f, XR24,
            "preference is the consumer's, not the compositor's"
        );
        assert_eq!(m, vec![5]);
    }

    #[test]
    fn modifier_lists_are_scoped_to_their_exact_fourcc() {
        let offered = vec![(XR24, vec![7]), (AR24, vec![3])];
        let importable = vec![(XR24, vec![9]), (AR24, vec![3])];
        assert_eq!(
            choose_format(&offered, &[XR24, AR24], &importable),
            Some((AR24, vec![3]))
        );
    }

    #[test]
    fn missing_or_empty_consumer_lists_reject_the_format() {
        let offered = vec![(XR24, vec![7])];
        assert!(choose_format(&offered, &[XR24], &[]).is_none());
        assert!(choose_format(&offered, &[XR24], &[(XR24, Vec::new())]).is_none());
    }

    #[test]
    fn linear_is_offered_first_because_every_consumer_imports_it() {
        let offered = vec![(XR24, vec![0x0100_0000_0000_0002, 0, 5])];
        let importable = vec![(XR24, vec![5, 0x0100_0000_0000_0002, 0])];
        let (_, m) = choose_format(&offered, &[XR24], &importable).unwrap();
        assert_eq!(m[0], 0, "LINEAR must lead the allocation attempt");
    }

    #[test]
    fn only_the_packed_rgb_fourccs_the_encoders_ingest_map_to_a_pixel_format() {
        assert_eq!(fourcc_to_pixel(XR24), Some(PixelFormat::Bgrx));
        assert_eq!(fourcc_to_pixel(AR24), Some(PixelFormat::Bgra));
        assert_eq!(fourcc_to_pixel(NV12), None);
        assert_eq!(fourcc_to_pixel(P010), None);
    }
}
