use super::*;
use crate::smoke_d3d11::nv12_texture;

/// An IDR empties the mirror, drops a queued force and marks slot 0.
#[test]
fn an_idr_resets_the_ltr_mirror_and_marks_slot_zero() {
    let (mut slots, mut next, mut pending) = ([Some(3), Some(5)], 1, Some(1));
    let step = ltr_step(&mut slots, &mut next, &mut pending, true, 9, 8, None, false);
    assert_eq!(
        step,
        LtrStep {
            mark_slot: Some(0),
            ..LtrStep::default()
        }
    );
    assert_eq!((slots, next, pending), ([Some(9), None], 1, None));
}

/// A queued force on a marked slot re-references it, clears the other slot and takes the
/// frame's mark. On a slot the taint sweep emptied it ships a plain P.
#[test]
fn a_queued_force_needs_a_marked_slot() {
    let (mut slots, mut next, mut pending) = ([Some(0), Some(8)], 0, Some(0));
    let step = ltr_step(
        &mut slots,
        &mut next,
        &mut pending,
        false,
        16,
        8,
        None,
        false,
    );
    assert_eq!(
        step,
        LtrStep {
            force_slot: Some(0),
            ..LtrStep::default()
        }
    );
    assert_eq!((slots, pending), ([Some(0), None], None));
    let (mut slots, mut pending) = ([None, Some(8)], Some(0));
    let step = ltr_step(
        &mut slots,
        &mut next,
        &mut pending,
        false,
        17,
        8,
        None,
        false,
    );
    assert_eq!(step, LtrStep::default());
    assert_eq!(pending, None, "a force is consumed either way");
}

/// Under confirmed references a frame forces the newest confirmed slot, which clears
/// the other, and marks into it. With none confirmed and both awaiting it, nothing.
#[test]
fn confirmed_references_force_the_newest_confirmed_slot() {
    let (mut slots, mut next, mut pending) = ([Some(9), Some(10)], 0, None);
    let step = ltr_step(
        &mut slots,
        &mut next,
        &mut pending,
        false,
        11,
        8,
        Some(crate::Acked {
            last: 10,
            mask: 0xffff,
        }),
        false,
    );
    assert_eq!(
        step,
        LtrStep {
            mark_slot: Some(0),
            force_slot: Some(1),
            acked: true
        }
    );
    assert_eq!((slots, next), ([Some(11), Some(10)], 1));
    let step = ltr_step(
        &mut slots,
        &mut next,
        &mut pending,
        false,
        12,
        8,
        Some(crate::Acked {
            last: 9,
            mask: 0xffff,
        }),
        false,
    );
    assert_eq!((step.mark_slot, step.force_slot), (None, None));
}

/// A driver that keeps unforced slots keeps a mark awaiting confirmation: the frame
/// marks only a free slot, and the next one forces the newer confirmed frame.
#[test]
fn kept_slots_hold_a_mark_until_it_is_confirmed() {
    let (mut slots, mut next, mut pending) = ([Some(9), Some(10)], 0, None);
    let step = ltr_step(
        &mut slots,
        &mut next,
        &mut pending,
        false,
        11,
        8,
        Some(crate::Acked {
            last: 9,
            mask: 0xffff,
        }),
        true,
    );
    assert_eq!((step.mark_slot, step.force_slot), (None, Some(0)));
    assert_eq!(slots, [Some(9), Some(10)], "10 awaits its confirmation");
    let step = ltr_step(
        &mut slots,
        &mut next,
        &mut pending,
        false,
        12,
        8,
        Some(crate::Acked {
            last: 10,
            mask: 0xffff,
        }),
        true,
    );
    assert_eq!((step.mark_slot, step.force_slot), (Some(0), Some(1)));
    assert_eq!(slots, [Some(12), Some(10)]);
}

/// Marks land on the interval, first on an empty slot, else round robin.
#[test]
fn a_mark_prefers_an_empty_slot() {
    let (mut slots, mut next, mut pending) = ([Some(0), None], 0, None);
    let step = ltr_step(
        &mut slots,
        &mut next,
        &mut pending,
        false,
        15,
        8,
        None,
        false,
    );
    assert_eq!(step, LtrStep::default(), "off the interval");
    let step = ltr_step(
        &mut slots,
        &mut next,
        &mut pending,
        false,
        16,
        8,
        None,
        false,
    );
    assert_eq!(step.mark_slot, Some(1));
    let step = ltr_step(
        &mut slots,
        &mut next,
        &mut pending,
        false,
        24,
        8,
        None,
        false,
    );
    assert_eq!(step.mark_slot, Some(0), "both marked: the round robin");
    assert_eq!(slots, [Some(24), Some(16)]);
}

// Layout of the FFI mirrors lives as `const _: ()` in `amf_sys.rs` (every build). This
// checks little-endian union payload packing, which a size/align assert cannot express.
#[test]
fn variant_payload_packing_matches_c() {
    let v = AmfVariant::from_rate(60, 1);
    assert_eq!(v.payload[0], 60u64 | (1u64 << 32));
    assert_eq!(AmfVariant::from_i64(-1).payload[0], u64::MAX);
}

/// HDR10 grade for live tests: BT.2020, 1000-nit, ST.2086 wire order (primaries G, B, R).
fn sample_hdr_meta() -> pf_frame::HdrMeta {
    pf_frame::HdrMeta {
        display_primaries: [[8500, 39850], [6550, 2300], [35400, 14600]],
        white_point: [15635, 16450],
        max_display_mastering_luminance: 1000 * 10000,
        min_display_mastering_luminance: 50,
        max_cll: 1000,
        max_fall: 400,
    }
}

/// The bind flags every AMF test texture takes.
const BIND_SR: u32 = windows::Win32::Graphics::Direct3D11::D3D11_BIND_SHADER_RESOURCE.0 as u32;

/// D3D11 device on the AMD adapter. `None` = no AMD GPU — caller skips.
fn amd_d3d11_device() -> Option<ID3D11Device> {
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0};
    use windows::Win32::Graphics::Direct3D11::{D3D11CreateDevice, D3D11_SDK_VERSION};
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1};
    const VENDOR_AMD: u32 = 0x1002;
    // SAFETY: probe owns every handle. Factory/adapter COM or err; CreateDevice fills
    // `device` only on success. Everything drops with its COM wrapper.
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().ok()?;
        for i in 0.. {
            let adapter: IDXGIAdapter1 = factory.EnumAdapters1(i).ok()?;
            let desc = adapter.GetDesc1().ok()?;
            if desc.VendorId != VENDOR_AMD {
                continue;
            }
            let mut device: Option<ID3D11Device> = None;
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                Default::default(),
                Some(&[D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                None,
            )
            .ok()?;
            return device;
        }
        None
    }
}

/// Live [`Encoder`] smoke per codec: submit/poll, native `reset()`, second batch, flush-drain.
/// Asserts Annex-B (or AV1 OBU), IDR at start and after reset, FIFO pts. Skips without AMD.
/// The driver answers SET_ENCODE — where the host latches these caps for the session — before
/// any frame is submitted, so what `caps()` says at that moment must be what the encoder
/// negotiated. LTR and intra-refresh are decided in `ensure_inner`, which used to run only at
/// the first submit: the host therefore latched `supports_rfi: false` and never sent a
/// reference-frame invalidation, costing a full IDR per lost frame.
///
/// Hardware-independent: it compares the two reads rather than demanding LTR, so a GPU that
/// genuinely declines still passes (and the printed values say which happened).
/// A skipped frame repeats the reference; under the one-frame VBV that is the picture
/// freezing on a scene cut, so the open must leave the switch off whatever the usage set.
#[test]
fn amf_frame_skip_is_off_after_open_live() {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();
    if let Err(e) = try_factory() {
        eprintln!("skipping: AMF runtime unavailable ({e})");
        return;
    }
    let Some(device) = amd_d3d11_device() else {
        eprintln!("skipping: no AMD adapter on this box");
        return;
    };
    let mut enc = match AmfEncoder::open(
        Codec::H265,
        PixelFormat::Nv12,
        640,
        480,
        60,
        2_000_000,
        8,
        ChromaFormat::Yuv420,
        false,
        None,
    ) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("skipping: native AMF open declined ({e:#})");
            return;
        }
    };
    enc.prepare(&device).expect("prepare");
    let comp = &enc
        .inner
        .as_ref()
        .expect("prepare opened the component")
        .comp;
    let skip = comp.get_prop_bool(enc.props.skip_frame);
    assert_eq!(
        skip,
        Some(false),
        "rate-control frame skip must be off after open"
    );
}

#[test]
fn amf_caps_do_not_change_at_the_first_submit_live() {
    if let Err(e) = try_factory() {
        eprintln!("skipping: AMF runtime unavailable ({e})");
        return;
    }
    let Some(device) = amd_d3d11_device() else {
        eprintln!("skipping: no AMD adapter on this box");
        return;
    };
    let (w, h, fps) = (640u32, 480u32, 60u32);
    let tex = nv12_texture(&device, w, h, None, BIND_SR);
    let mut enc = match AmfEncoder::open(
        Codec::H264,
        PixelFormat::Nv12,
        w,
        h,
        fps,
        2_000_000,
        8,
        ChromaFormat::Yuv420,
        false,
        None,
    ) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("skipping: native AMF open declined ({e:#})");
            return;
        }
    };
    enc.prepare(&device).expect("prepare");
    let at_open = enc.caps();
    let frame = CapturedFrame {
        provenance: Default::default(),
        width: w,
        height: h,
        pts_ns: 1,
        format: PixelFormat::Nv12,
        payload: FramePayload::D3d11(pf_frame::dxgi::D3d11Frame {
            texture: tex.clone(),
            device: device.clone(),
            pyro: None,
        }),
        cursor: None,
    };
    enc.submit(&frame).expect("submit");
    let _ = enc.poll().expect("poll");
    let after = enc.caps();
    eprintln!(
        "AMF caps at open: rfi={} ir={} | after first submit: rfi={} ir={}",
        at_open.supports_rfi, at_open.intra_refresh, after.supports_rfi, after.intra_refresh
    );
    assert_eq!(
        (at_open.supports_rfi, at_open.intra_refresh),
        (after.supports_rfi, after.intra_refresh),
        "the host reads these once, before the first frame"
    );
}

/// `flush` drains the component, which leaves it at end-of-stream where it takes no more
/// input. A session that flushed must encode again, and every AU on both sides of the
/// flush must carry the pts of the frame it encodes.
#[test]
fn amf_encodes_again_after_a_flush_live() {
    if let Err(e) = try_factory() {
        eprintln!("skipping: AMF runtime unavailable ({e})");
        return;
    }
    let Some(device) = amd_d3d11_device() else {
        eprintln!("skipping: no AMD adapter on this box");
        return;
    };
    let (w, h, fps) = (640u32, 480u32, 60u32);
    let tex = nv12_texture(&device, w, h, None, BIND_SR);
    let mut enc = match AmfEncoder::open(
        Codec::H264,
        PixelFormat::Nv12,
        w,
        h,
        fps,
        2_000_000,
        8,
        ChromaFormat::Yuv420,
        false,
        None,
    ) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("skipping: native AMF open declined ({e:#})");
            return;
        }
    };
    enc.prepare(&device).expect("prepare");
    let run = |enc: &mut AmfEncoder, base: u64| {
        let mut pts = Vec::new();
        for i in 0..8 {
            let frame = CapturedFrame {
                provenance: Default::default(),
                width: w,
                height: h,
                pts_ns: base + i,
                format: PixelFormat::Nv12,
                payload: FramePayload::D3d11(pf_frame::dxgi::D3d11Frame {
                    texture: tex.clone(),
                    device: device.clone(),
                    pyro: None,
                }),
                cursor: None,
            };
            enc.submit(&frame).expect("submit");
            if let Some(au) = enc.poll().expect("poll") {
                pts.push(au.pts_ns);
            }
        }
        pts
    };
    let before = run(&mut enc, 1);
    assert!(
        !before.is_empty(),
        "the encoder produced nothing before the flush"
    );
    enc.flush().expect("flush");
    let after = run(&mut enc, 1000);
    eprintln!("AMF AUs before flush: {before:?}, after: {after:?}");
    let resumed: Vec<u64> = after.iter().copied().filter(|p| *p >= 1000).collect();
    assert!(
        !resumed.is_empty(),
        "the encoder accepted no input after a flush — it is still at end-of-stream"
    );
    assert!(
        after
            .iter()
            .all(|p| (1..=8).contains(p) || (1000..1008).contains(p)),
        "an AU after the flush carries a pts nobody submitted: {after:?}"
    );
    assert!(
        resumed.windows(2).all(|w| w[1] == w[0] + 1),
        "AUs after the flush are paired off by one: {resumed:?}"
    );
}

/// Live BGRA input per codec: VCN converts, so the encoder takes what the display
/// composes. Both submit modes must return an access unit per frame — the ring copy, and
/// the caller's own texture in place, which is how the driver's pool submits. Skips
/// without AMD.
#[test]
fn amf_bgra_encode_live_smoke() {
    use crate::smoke_d3d11::bgra_texture;
    use crate::smoke_pattern::scroll_pattern;
    if let Err(e) = try_factory() {
        eprintln!("skipping: AMF runtime unavailable ({e})");
        return;
    }
    let Some(device) = amd_d3d11_device() else {
        eprintln!("skipping: no AMD adapter on this box");
        return;
    };
    let (w, h, fps) = (640u32, 480u32, 60u32);
    // The binds of the driver's BGRA pool slots.
    let bind = BIND_SR | D3D11_BIND_RENDER_TARGET.0 as u32;
    let texs: Vec<ID3D11Texture2D> = (0..3)
        .map(|i| {
            let px = scroll_pattern(w as usize, h as usize, i);
            bgra_texture(&device, w, h, Some(&px), bind)
        })
        .collect();
    const FRAMES: usize = 12;
    for in_place in [false, true] {
        for codec in [Codec::H265, Codec::H264, Codec::Av1] {
            if codec == Codec::Av1 && !probe_can_encode_on(&device, codec) {
                eprintln!("skipping Av1: this AMD GPU's native probe declined it");
                continue;
            }
            let mut enc = AmfEncoder::open(
                codec,
                PixelFormat::Bgra,
                w,
                h,
                fps,
                2_000_000,
                8,
                ChromaFormat::Yuv420,
                false,
                None,
            )
            .expect("open on BGRA");
            if in_place {
                // Three textures in rotation, two in flight: the third is always free.
                enc.set_input_ring_depth(2);
            }
            let batch = |enc: &mut AmfEncoder, base: u64| -> Vec<EncodedFrame> {
                let mut aus = Vec::new();
                for i in 0..FRAMES {
                    let frame = CapturedFrame {
                        provenance: Default::default(),
                        width: w,
                        height: h,
                        pts_ns: base + i as u64,
                        format: PixelFormat::Bgra,
                        payload: FramePayload::D3d11(pf_frame::dxgi::D3d11Frame {
                            texture: texs[i % texs.len()].clone(),
                            device: device.clone(),
                            pyro: None,
                        }),
                        cursor: None,
                    };
                    enc.submit(&frame).expect("submit");
                    if let Some(au) = enc.poll().expect("poll") {
                        aus.push(au);
                    }
                }
                enc.flush().expect("flush");
                for _ in 0..50 {
                    match enc.poll().expect("drain poll") {
                        Some(au) => aus.push(au),
                        None => break,
                    }
                }
                aus
            };
            let aus = batch(&mut enc, 1);
            eprintln!(
                "{codec:?} in_place={in_place}: {} AUs of {FRAMES}, {} bytes",
                aus.len(),
                aus.iter().map(|a| a.data.len()).sum::<usize>()
            );
            assert_eq!(aus.len(), FRAMES, "{codec:?} in_place={in_place}");
            assert!(aus[0].keyframe, "{codec:?}: the stream starts on an IDR");
            assert_eq!(aus[0].pts_ns, 1, "FIFO pts pairing");
            // A stall recovery re-Inits the component, which must still take BGRA.
            assert!(enc.reset(), "{codec:?}: reset rebuilds in place");
            let again = batch(&mut enc, 100);
            assert_eq!(
                again.len(),
                FRAMES,
                "{codec:?} in_place={in_place}: after a reset"
            );
            assert!(again[0].keyframe, "{codec:?}: a reset restarts on an IDR");
        }
    }
}

/// Live FP16 input per ten-bit codec: VCN takes the scRGB an HDR desktop composes and
/// converts it itself. An access unit per frame in both submit modes, and again after a
/// reset. HEVC must take it; an AV1 that declines is the driver's P010 fallback, so it
/// is reported and skipped. Skips without AMD.
#[test]
fn amf_fp16_hdr_encode_live() {
    use windows::Win32::Graphics::Direct3D11::D3D11_SUBRESOURCE_DATA;
    if let Err(e) = try_factory() {
        eprintln!("skipping: AMF runtime unavailable ({e})");
        return;
    }
    let Some(device) = amd_d3d11_device() else {
        eprintln!("skipping: no AMD adapter on this box");
        return;
    };
    let (w, h, fps) = (640u32, 480u32, 60u32);
    // Flat scRGB white at 80, 160 and 320 nits, as IEEE halves (1.0, 2.0, 4.0).
    let texs: Vec<ID3D11Texture2D> = [0x3C00u16, 0x4000, 0x4400]
        .iter()
        .map(|&level| {
            let px = vec![level; (w * h * 4) as usize];
            let desc = D3D11_TEXTURE2D_DESC {
                Width: w,
                Height: h,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_R16G16B16A16_FLOAT,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DEFAULT,
                // The binds of the driver's pool slots.
                BindFlags: BIND_SR | D3D11_BIND_RENDER_TARGET.0 as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let init = D3D11_SUBRESOURCE_DATA {
                pSysMem: px.as_ptr().cast(),
                SysMemPitch: w * 8,
                SysMemSlicePitch: 0,
            };
            let mut tex = None;
            // SAFETY: `init` points at `px`, `w * h * 8` bytes alive across the call, read
            // at the pitch given. The out-param fills only on success.
            unsafe { device.CreateTexture2D(&desc, Some(&init), Some(&mut tex)) }
                .expect("FP16 texture");
            tex.expect("FP16 texture")
        })
        .collect();
    const FRAMES: usize = 12;
    for in_place in [false, true] {
        for codec in [Codec::H265, Codec::Av1] {
            if codec == Codec::Av1 && !probe_can_encode_on(&device, codec) {
                eprintln!("skipping Av1: this AMD GPU's native probe declined it");
                continue;
            }
            let mut enc = AmfEncoder::open(
                codec,
                PixelFormat::RgbaF16,
                w,
                h,
                fps,
                2_000_000,
                10,
                ChromaFormat::Yuv420,
                true,
                None,
            )
            .expect("open on FP16");
            if let Err(e) = enc.prepare(&device) {
                assert_ne!(codec, Codec::H265, "HEVC Main10 declined FP16: {e:#}");
                eprintln!("skipping {codec:?}: FP16 input declined ({e:#})");
                continue;
            }
            if in_place {
                // Three textures in rotation, two in flight: the third is always free.
                enc.set_input_ring_depth(2);
            }
            let batch = |enc: &mut AmfEncoder, base: u64| -> Vec<EncodedFrame> {
                let mut aus = Vec::new();
                for i in 0..FRAMES {
                    let frame = CapturedFrame {
                        provenance: Default::default(),
                        width: w,
                        height: h,
                        pts_ns: base + i as u64,
                        format: PixelFormat::RgbaF16,
                        payload: FramePayload::D3d11(pf_frame::dxgi::D3d11Frame {
                            texture: texs[i % texs.len()].clone(),
                            device: device.clone(),
                            pyro: None,
                        }),
                        cursor: None,
                    };
                    enc.submit(&frame).expect("submit");
                    if let Some(au) = enc.poll().expect("poll") {
                        aus.push(au);
                    }
                }
                enc.flush().expect("flush");
                for _ in 0..50 {
                    match enc.poll().expect("drain poll") {
                        Some(au) => aus.push(au),
                        None => break,
                    }
                }
                aus
            };
            let aus = batch(&mut enc, 1);
            eprintln!(
                "{codec:?} FP16 in_place={in_place}: {} AUs of {FRAMES}, {} bytes",
                aus.len(),
                aus.iter().map(|a| a.data.len()).sum::<usize>()
            );
            assert_eq!(aus.len(), FRAMES, "{codec:?} in_place={in_place}");
            assert!(aus[0].keyframe, "{codec:?}: the stream starts on an IDR");
            assert!(enc.reset(), "{codec:?}: reset rebuilds in place");
            let again = batch(&mut enc, 100);
            assert_eq!(
                again.len(),
                FRAMES,
                "{codec:?} in_place={in_place}: after a reset"
            );
        }
    }
}

/// An open encoder with nothing to encode costs no CPU: its retrieve thread parks in
/// `QueryOutput` or samples it, never spins on it. Skips without AMD.
#[test]
fn amf_idle_encoder_does_not_spin_live() {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    if let Err(e) = try_factory() {
        eprintln!("skipping: AMF runtime unavailable ({e})");
        return;
    }
    let Some(device) = amd_d3d11_device() else {
        eprintln!("skipping: no AMD adapter on this box");
        return;
    };
    // Kernel plus user time of this process, in 100 ns units.
    let cpu = || {
        let mut t = [FILETIME::default(); 4];
        let [created, exited, kernel, user] = &mut t;
        // SAFETY: the pseudo-handle is always valid; the four out-params are locals.
        unsafe { GetProcessTimes(GetCurrentProcess(), created, exited, kernel, user) }
            .expect("GetProcessTimes");
        let ticks = |f: FILETIME| (u64::from(f.dwHighDateTime) << 32) | u64::from(f.dwLowDateTime);
        ticks(t[2]) + ticks(t[3])
    };
    let (w, h) = (640u32, 480u32);
    let tex = nv12_texture(&device, w, h, None, BIND_SR);
    for codec in [Codec::H265, Codec::H264] {
        let mut enc = AmfEncoder::open(
            codec,
            PixelFormat::Nv12,
            w,
            h,
            60,
            2_000_000,
            8,
            ChromaFormat::Yuv420,
            false,
            None,
        )
        .expect("open");
        let frame = CapturedFrame {
            provenance: Default::default(),
            width: w,
            height: h,
            pts_ns: 1,
            format: PixelFormat::Nv12,
            payload: FramePayload::D3d11(pf_frame::dxgi::D3d11Frame {
                texture: tex.clone(),
                device: device.clone(),
                pyro: None,
            }),
            cursor: None,
        };
        enc.submit(&frame).expect("submit");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while enc.poll().expect("poll").is_none() {
            assert!(std::time::Instant::now() < deadline, "{codec:?}: no AU");
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        // The quietest of three windows, so another test's burst cannot fail this one.
        let idle_ms = (0..3)
            .map(|_| {
                let before = cpu();
                std::thread::sleep(std::time::Duration::from_millis(200));
                (cpu() - before) / 10_000
            })
            .min()
            .unwrap_or(0);
        eprintln!("{codec:?}: {idle_ms} ms of CPU in 200 ms idle");
        assert!(
            idle_ms < 60,
            "{codec:?}: the idle encoder burned {idle_ms} ms of 200"
        );
    }
}

#[test]
fn amf_encode_live_smoke() {
    if let Err(e) = try_factory() {
        eprintln!("skipping: AMF runtime unavailable ({e})");
        return;
    }
    let Some(device) = amd_d3d11_device() else {
        eprintln!("skipping: no AMD adapter on this box");
        return;
    };
    let (w, h, fps) = (640u32, 480u32, 60u32);
    let tex = nv12_texture(&device, w, h, None, BIND_SR);

    for codec in [Codec::H265, Codec::H264, Codec::Av1] {
        // AV1 is RDNA3+: probe THIS device (`open` may pick a different GPU on a hybrid box).
        if codec == Codec::Av1 && !probe_can_encode_on(&device, codec) {
            eprintln!("skipping Av1: this AMD GPU's native probe declined it (pre-RDNA3?)");
            continue;
        }
        let mut enc = match AmfEncoder::open(
            codec,
            PixelFormat::Nv12,
            w,
            h,
            fps,
            2_000_000,
            8,
            ChromaFormat::Yuv420,
            false,
            None,
        ) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("skipping {codec:?}: native AMF open declined ({e:#})");
                continue;
            }
        };
        let batch = |enc: &mut AmfEncoder, base: u64, n: usize| -> Vec<EncodedFrame> {
            let mut aus = Vec::new();
            for i in 0..n {
                let frame = CapturedFrame {
                    provenance: Default::default(),
                    width: w,
                    height: h,
                    pts_ns: base + i as u64,
                    format: PixelFormat::Nv12,
                    payload: FramePayload::D3d11(pf_frame::dxgi::D3d11Frame {
                        texture: tex.clone(),
                        device: device.clone(),
                        pyro: None,
                    }),
                    cursor: None,
                };
                enc.submit(&frame).expect("submit");
                if let Some(au) = enc.poll().expect("poll") {
                    aus.push(au);
                }
            }
            aus
        };
        let first_run = batch(&mut enc, 1, 6);
        assert!(enc.reset(), "native reset must report rebuilt");
        let mut second_run = batch(&mut enc, 100, 6);
        enc.flush().expect("flush");
        for _ in 0..50 {
            match enc.poll().expect("drain poll") {
                Some(au) => second_run.push(au),
                None => break,
            }
        }
        assert!(
            first_run.len() >= 3 && second_run.len() >= 3,
            "{codec:?}: expected most AUs out (got {} + {})",
            first_run.len(),
            second_run.len()
        );
        for run in [&first_run, &second_run] {
            let first = &run[0];
            assert!(
                first.keyframe,
                "{codec:?}: stream/reset start must be an IDR"
            );
            if codec == Codec::Av1 {
                // AV1 is OBU, not Annex-B.
                assert!(!first.data.is_empty(), "Av1: empty key AU");
            } else {
                assert!(
                    first.data.starts_with(&[0, 0, 0, 1]) || first.data.starts_with(&[0, 0, 1]),
                    "{codec:?}: AU must be Annex-B (got {:02x?})",
                    &first.data[..first.data.len().min(8)]
                );
            }
        }
        assert_eq!(first_run[0].pts_ns, 1, "FIFO pts pairing");
        // Bitstream FIFO: a declined B-frame pin would reorder AUs. Don't trust set_prop.
        for run in [&first_run, &second_run] {
            for pair in run.windows(2) {
                assert!(
                    pair[1].pts_ns > pair[0].pts_ns,
                    "{codec:?}: AUs must leave in submit order (reordering ⇒ B-frames), \
                     got {} then {}",
                    pair[0].pts_ns,
                    pair[1].pts_ns
                );
            }
        }
        assert_eq!(second_run[0].pts_ns, 100, "post-reset FIFO pts pairing");
        eprintln!(
            "live AMF {codec:?} encode: {} + {} AUs across a native reset, first IDR {} bytes",
            first_run.len(),
            second_run.len(),
            first_run[0].data.len()
        );
    }
}

/// A submit refused after the LTR decision — surface creation, a property set — must not
/// leave the mirror claiming a mark the hardware never made, nor eat a queued force: the
/// next frame is an IDR, which resets both. Refuses one mark frame and one recovery frame.
/// Skips without AMD; the mirror checks skip when the driver declines LTR.
#[test]
fn amf_refused_submit_forces_idr_live() {
    if let Err(e) = try_factory() {
        eprintln!("skipping: AMF runtime unavailable ({e})");
        return;
    }
    let Some(device) = amd_d3d11_device() else {
        eprintln!("skipping: no AMD adapter on this box");
        return;
    };
    let (w, h) = (640u32, 480u32);
    let tex = nv12_texture(&device, w, h, None, BIND_SR);
    let mut enc = match AmfEncoder::open(
        Codec::H264,
        PixelFormat::Nv12,
        w,
        h,
        30,
        2_000_000,
        8,
        ChromaFormat::Yuv420,
        false,
        None,
    ) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("skipping: native AMF open declined ({e:#})");
            return;
        }
    };
    enc.prepare(&device).expect("prepare");
    let ltr = enc.caps().supports_rfi;
    let mark = enc.ltr_mark_interval as u32;
    assert!(
        mark >= 4,
        "mark interval {mark} leaves no room for a loss window"
    );
    let (refused_mark, refused_force) = (mark, 2 * mark);
    let mut aus: Vec<EncodedFrame> = Vec::new();
    for i in 0..4 * mark {
        if i == refused_mark {
            enc.fail_submit_at = Some(i as i64);
        }
        if i == refused_force {
            if ltr {
                // The IDR after the refused mark is the pre-loss anchor.
                assert!(
                    enc.invalidate_ref_frames(i as i64 - 2, i as i64 - 1),
                    "no pre-loss LTR to force"
                );
            }
            enc.fail_submit_at = Some(i as i64);
        }
        let frame = CapturedFrame {
            provenance: Default::default(),
            width: w,
            height: h,
            pts_ns: i as u64,
            format: PixelFormat::Nv12,
            payload: FramePayload::D3d11(pf_frame::dxgi::D3d11Frame {
                texture: tex.clone(),
                device: device.clone(),
                pyro: None,
            }),
            cursor: None,
        };
        match enc.submit_indexed(&frame, i) {
            Ok(()) => assert_ne!(enc.fail_submit_at, Some(i as i64), "frame {i} not refused"),
            Err(e) => assert_eq!(enc.fail_submit_at, Some(i as i64), "submit {i}: {e:#}"),
        }
        if let Some(au) = enc.poll().expect("poll") {
            aus.push(au);
        }
    }
    enc.flush().expect("flush");
    while let Some(au) = enc.poll().expect("drain") {
        aus.push(au);
    }
    let au = |i: u32| {
        aus.iter()
            .find(|a| a.pts_ns == i as u64)
            .unwrap_or_else(|| panic!("no AU for frame {i}"))
    };
    assert!(au(0).keyframe, "first AU must be a keyframe");
    for refused in [refused_mark, refused_force] {
        assert!(
            aus.iter().all(|a| a.pts_ns != refused as u64),
            "refused frame {refused} produced an AU"
        );
        assert!(
            au(refused + 1).keyframe,
            "frame {} after refused frame {refused} must be an IDR",
            refused + 1
        );
    }
    if ltr {
        assert!(
            !enc.ltr_slots.contains(&Some(refused_mark as i64)),
            "mirror claims a mark the hardware never made: {:?}",
            enc.ltr_slots
        );
    }
    eprintln!(
        "live AMF refused-submit: {} AUs, ltr={ltr}, mirror {:?}",
        aus.len(),
        enc.ltr_slots
    );
}

/// LTR anchors on hardware: the wave smokes' moving pattern with losses answered through
/// `invalidate_ref_frames`, shaped and dumped by [`crate::smoke_pattern::Soak`].
///
/// `cargo test -p pf-encode-win --lib amf_ltr_anchor_soak -- --ignored --nocapture`
#[test]
#[ignore = "requires an AMD GPU with AMF — run manually on an AMD Windows box (.173)"]
fn amf_ltr_anchor_soak() {
    use crate::{smoke_d3d11::nv12_scroll_frame, smoke_pattern::Soak};
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();
    try_factory().expect("AMF runtime");
    let device = amd_d3d11_device().expect("an AMD adapter");
    let soak = Soak::from_env();
    let mut enc = AmfEncoder::open(
        soak.codec,
        PixelFormat::Nv12,
        soak.w,
        soak.h,
        soak.fps,
        soak.mbps * 1_000_000,
        8,
        ChromaFormat::Yuv420,
        false,
        None,
    )
    .expect("AMF open");
    enc.prepare(&device).expect("prepare");
    assert!(
        enc.caps().supports_rfi,
        "the driver declined LTR: nothing to soak"
    );
    println!(
        "amf_ltr_anchor_soak: LTR mark interval {} keep {}",
        enc.ltr_mark_interval, enc.ltr_keep
    );
    let (w, h) = (soak.w, soak.h);
    soak.run("amf", &mut enc, |i| {
        nv12_scroll_frame(&device, w, h, i, BIND_SR)
    });
}

/// Live `applied_bitrate_bps`: None before lazy open, open rate after submit, new rate after
/// retarget. Skips without AMD.
#[test]
fn amf_applied_bitrate_readback_live() {
    if let Err(e) = try_factory() {
        eprintln!("skipping: AMF runtime unavailable ({e})");
        return;
    }
    let Some(device) = amd_d3d11_device() else {
        eprintln!("skipping: no AMD adapter on this box");
        return;
    };
    let (w, h, fps) = (640u32, 480u32, 60u32);
    let tex = nv12_texture(&device, w, h, None, BIND_SR);
    let mut enc = AmfEncoder::open(
        Codec::H265,
        PixelFormat::Nv12,
        w,
        h,
        fps,
        2_000_000,
        8,
        ChromaFormat::Yuv420,
        false,
        None,
    )
    .expect("native AMF open");
    assert_eq!(
        enc.applied_bitrate_bps(),
        None,
        "no readback before the lazy open — the caller must keep the requested rate"
    );
    let frame = CapturedFrame {
        provenance: Default::default(),
        width: w,
        height: h,
        pts_ns: 1,
        format: PixelFormat::Nv12,
        payload: FramePayload::D3d11(pf_frame::dxgi::D3d11Frame {
            texture: tex.clone(),
            device: device.clone(),
            pyro: None,
        }),
        cursor: None,
    };
    enc.submit(&frame).expect("submit");
    let opened = enc.applied_bitrate_bps();
    assert_eq!(
        opened,
        Some(2_000_000),
        "post-open readback must be the accepted open rate"
    );
    assert!(
        enc.reconfigure_bitrate(8_000_000),
        "dynamic retarget declined on live hardware"
    );
    let retargeted = enc.applied_bitrate_bps();
    assert_eq!(
        retargeted,
        Some(8_000_000),
        "post-retarget readback must be the accepted NEW rate"
    );
    eprintln!("live AMF applied-bitrate readback: open {opened:?} -> retarget {retargeted:?}");
}

/// Live: a rate no VCN takes opens at the encoder's ceiling, and so does a retarget.
/// Skips without AMD.
#[test]
fn amf_rate_over_ceiling_live() {
    if let Err(e) = try_factory() {
        eprintln!("skipping: AMF runtime unavailable ({e})");
        return;
    }
    let Some(device) = amd_d3d11_device() else {
        eprintln!("skipping: no AMD adapter on this box");
        return;
    };
    const ASK: u64 = 2_600_000_000;
    for codec in [Codec::H264, Codec::H265, Codec::Av1] {
        let opened = AmfEncoder::open(
            codec,
            PixelFormat::Nv12,
            1920,
            1080,
            60,
            ASK,
            8,
            ChromaFormat::Yuv420,
            false,
            None,
        );
        let Ok(mut enc) = opened else {
            eprintln!("skipping {codec:?}: this GPU declined it");
            continue;
        };
        enc.prepare(&device).expect("open over the ceiling");
        let ceiling = enc.applied_bitrate_bps().expect("readback");
        assert!(ceiling < ASK, "{codec:?} took {ASK}");
        assert!(enc.reconfigure_bitrate(20_000_000), "{codec:?} retarget");
        assert!(
            enc.reconfigure_bitrate(ASK),
            "{codec:?} retarget over the ceiling"
        );
        assert_eq!(enc.applied_bitrate_bps(), Some(ceiling), "{codec:?}");
        eprintln!("live AMF ceiling {codec:?}: {} Mbit/s", ceiling / 1_000_000);
    }
}

/// Live probe: AVC and HEVC must be true on any VCN; AV1 is hardware truth (RDNA3+).
#[test]
fn amf_native_probe_live() {
    if let Err(e) = try_factory() {
        eprintln!("skipping: AMF runtime unavailable ({e})");
        return;
    }
    let Some(device) = amd_d3d11_device() else {
        eprintln!("skipping: no AMD adapter on this box");
        return;
    };
    let h264 = probe_can_encode_on(&device, Codec::H264);
    let h265 = probe_can_encode_on(&device, Codec::H265);
    let av1 = probe_can_encode_on(&device, Codec::Av1);
    eprintln!("native AMF probe: h264={h264} h265={h265} av1={av1}");
    assert!(h264 && h265, "every VCN generation encodes AVC + HEVC");
}

/// Live HDR: P010 HEVC Main10 must encode. Mastering/CLL prefix SEI (payload 137/144) is
/// soft-reported — VCN generations differ.
#[test]
fn amf_hdr_encode_live_smoke() {
    use windows::Win32::Graphics::Direct3D11::D3D11_BIND_SHADER_RESOURCE;
    if let Err(e) = try_factory() {
        eprintln!("skipping: AMF runtime unavailable ({e})");
        return;
    }
    let Some(device) = amd_d3d11_device() else {
        eprintln!("skipping: no AMD adapter on this box");
        return;
    };
    let (w, h, fps) = (640u32, 480u32, 60u32);
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_P010,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut tex: Option<ID3D11Texture2D> = None;
    // SAFETY: CreateTexture2D fills the out-param only on success; owned COM, this thread.
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut tex)) }.expect("P010 texture");
    let tex = tex.expect("P010 texture");
    let mut enc = match AmfEncoder::open(
        Codec::H265,
        PixelFormat::P010,
        w,
        h,
        fps,
        4_000_000,
        10,
        ChromaFormat::Yuv420,
        true,
        None,
    ) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("skipping: native AMF 10-bit open declined ({e:#})");
            return;
        }
    };
    enc.set_hdr_meta(Some(sample_hdr_meta()));
    let mut aus: Vec<EncodedFrame> = Vec::new();
    for i in 0..6 {
        let frame = CapturedFrame {
            provenance: Default::default(),
            width: w,
            height: h,
            pts_ns: 1 + i as u64,
            format: PixelFormat::P010,
            payload: FramePayload::D3d11(pf_frame::dxgi::D3d11Frame {
                texture: tex.clone(),
                device: device.clone(),
                pyro: None,
            }),
            cursor: None,
        };
        enc.submit(&frame).expect("submit (P010)");
        if let Some(au) = enc.poll().expect("poll") {
            aus.push(au);
        }
    }
    assert!(!aus.is_empty(), "10-bit HDR encode produced no AUs");
    let idr = &aus[0];
    assert!(idr.keyframe, "first AU must be an IDR");
    // HEVC prefix-SEI (NUH 0x4E 0x01): payload 137 mastering / 144 CLL.
    let mut mastering = false;
    let mut cll = false;
    for i in 0..idr.data.len().saturating_sub(5) {
        let d = &idr.data[i..];
        let nal = if d.starts_with(&[0, 0, 1]) {
            &d[3..]
        } else if d.starts_with(&[0, 0, 0, 1]) {
            &d[4..]
        } else {
            continue;
        };
        if nal.len() >= 3 && nal[0] == 0x4E && nal[1] == 0x01 {
            match nal[2] {
                137 => mastering = true,
                144 => cll = true,
                _ => {}
            }
        }
    }
    eprintln!(
        "live AMF HEVC Main10 HDR: {} AUs, IDR {} bytes, mastering SEI={mastering}, CLL SEI={cll}",
        aus.len(),
        idr.data.len()
    );
    if !mastering {
        eprintln!("note: no mastering-display SEI found on this VCN/driver — client falls back to the 0xCE datagram");
    }
}

/// Live 10-bit SDR: P010 HEVC Main10 under BT.709 (no HDR volume). Confirms the colour untie —
/// the encoder must NOT emit mastering/CLL SEI, and (via `AMF_SDR10_DUMP=<path>` + ffprobe) the
/// SPS VUI signals BT.709, not BT.2020 PQ. Same P010 ring as the HDR path; only the colour
/// differs.
#[test]
fn amf_sdr10_encode_live_smoke() {
    use windows::Win32::Graphics::Direct3D11::D3D11_BIND_SHADER_RESOURCE;
    if let Err(e) = try_factory() {
        eprintln!("skipping: AMF runtime unavailable ({e})");
        return;
    }
    let Some(device) = amd_d3d11_device() else {
        eprintln!("skipping: no AMD adapter on this box");
        return;
    };
    let (w, h, fps) = (640u32, 480u32, 60u32);
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_P010,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut tex: Option<ID3D11Texture2D> = None;
    // SAFETY: CreateTexture2D fills the out-param only on success; owned COM, this thread.
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut tex)) }.expect("P010 texture");
    let tex = tex.expect("P010 texture");
    let mut enc = match AmfEncoder::open(
        Codec::H265,
        PixelFormat::P010,
        w,
        h,
        fps,
        4_000_000,
        10,
        ChromaFormat::Yuv420,
        false, // SDR: BT.709, not BT.2020 PQ
        None,
    ) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("skipping: native AMF 10-bit SDR open declined ({e:#})");
            return;
        }
    };
    // No set_hdr_meta: a 10-bit SDR session carries no HDR volume.
    let mut aus: Vec<EncodedFrame> = Vec::new();
    for i in 0..6 {
        let frame = CapturedFrame {
            provenance: Default::default(),
            width: w,
            height: h,
            pts_ns: 1 + i as u64,
            format: PixelFormat::P010,
            payload: FramePayload::D3d11(pf_frame::dxgi::D3d11Frame {
                texture: tex.clone(),
                device: device.clone(),
                pyro: None,
            }),
            cursor: None,
        };
        enc.submit(&frame).expect("submit (P010 SDR)");
        if let Some(au) = enc.poll().expect("poll") {
            aus.push(au);
        }
    }
    assert!(!aus.is_empty(), "10-bit SDR encode produced no AUs");
    let idr = &aus[0];
    assert!(idr.keyframe, "first AU must be an IDR");
    // No mastering (137) / CLL (144) prefix SEI on an SDR stream.
    let mut hdr_sei = false;
    for i in 0..idr.data.len().saturating_sub(5) {
        let d = &idr.data[i..];
        let nal = if d.starts_with(&[0, 0, 1]) {
            &d[3..]
        } else if d.starts_with(&[0, 0, 0, 1]) {
            &d[4..]
        } else {
            continue;
        };
        if nal.len() >= 3 && nal[0] == 0x4E && nal[1] == 0x01 && matches!(nal[2], 137 | 144) {
            hdr_sei = true;
        }
    }
    assert!(
        !hdr_sei,
        "a 10-bit SDR stream must not carry HDR mastering/CLL SEI"
    );
    if let Ok(path) = std::env::var("AMF_SDR10_DUMP") {
        let full: Vec<u8> = aus.iter().flat_map(|a| a.data.iter().copied()).collect();
        let _ = std::fs::write(&path, &full);
        eprintln!(
            "amf_sdr10: wrote {path} ({} bytes, {} AUs)",
            full.len(),
            aus.len()
        );
    }
    eprintln!(
        "live AMF HEVC Main10 SDR: {} AUs, IDR {} bytes, hdr_sei={hdr_sei}",
        aus.len(),
        idr.data.len()
    );
}

/// Live: the D3D11 video processor converts 8-bit BGRA to a P010 target under BT.709 studio on
/// AMD. This is the SDR-10 converter half (`EncodeInput::P010Sdr`) — the "renders green" caveat
/// on RGB→P010 is NVIDIA-only, so prove AMD writes plausible luma. Mid-grey in → studio Y near
/// 504 (10-bit); a failed render is black (0) or clipped.
#[test]
fn videoconverter_bgra_to_p010_bt709_live() {
    use crate::convert::VideoConverter;
    use windows::Win32::Graphics::Direct3D11::{
        D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_CPU_ACCESS_READ,
        D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_SUBRESOURCE_DATA, D3D11_USAGE_STAGING,
    };
    use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
    let Some(device) = amd_d3d11_device() else {
        eprintln!("skipping: no AMD adapter on this box");
        return;
    };
    let (w, h) = (256u32, 256u32);
    // SAFETY: the device is live on this thread.
    let ctx = unsafe { device.GetImmediateContext() }.expect("immediate context");

    // BGRA filled mid-grey (128,128,128,255), one subresource upload.
    let pixels = vec![128u8; (w * h * 4) as usize];
    let bgra_desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        // Match the driver's captured-BGRA binds (`Targets::new` InputKind::Bgra); a
        // shader-resource-only texture is not a valid video-processor input surface on AMD.
        BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let init = D3D11_SUBRESOURCE_DATA {
        pSysMem: pixels.as_ptr() as *const _,
        SysMemPitch: w * 4,
        SysMemSlicePitch: 0,
    };
    let mut bgra: Option<ID3D11Texture2D> = None;
    // SAFETY: descriptor + init data are fully populated; out-param filled on success.
    unsafe { device.CreateTexture2D(&bgra_desc, Some(&init), Some(&mut bgra)) }
        .expect("BGRA texture");
    let bgra = bgra.expect("BGRA texture");

    let p010_desc = D3D11_TEXTURE2D_DESC {
        Format: DXGI_FORMAT_P010,
        BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
        ..bgra_desc
    };
    let mut p010: Option<ID3D11Texture2D> = None;
    // SAFETY: as above.
    unsafe { device.CreateTexture2D(&p010_desc, None, Some(&mut p010)) }.expect("P010 texture");
    let p010 = p010.expect("P010 texture");

    let conv = VideoConverter::new(&device, &ctx, w, h, false).expect("VideoConverter");
    // The load-bearing assertion: AMD's video processor accepts a P010 output view.
    conv.convert(&bgra, &p010)
        .expect("BGRA->P010 on the AMD video processor (a green render would still Ok here)");

    // Read back the Y plane's first sample through a staging copy.
    let stag_desc = D3D11_TEXTURE2D_DESC {
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        ..p010_desc
    };
    let mut stag: Option<ID3D11Texture2D> = None;
    // SAFETY: as above.
    unsafe { device.CreateTexture2D(&stag_desc, None, Some(&mut stag)) }.expect("staging P010");
    let stag = stag.expect("staging P010");
    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    // SAFETY: `stag` and `p010` are live same-device textures; `ctx` is their immediate context.
    // `Map` fills `mapped`; `Unmap` releases it before the function returns.
    let y10 = unsafe {
        ctx.CopyResource(&stag, &p010);
        ctx.Map(&stag, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
            .expect("map staging");
        // P010 Y plane: row 0, first 16-bit sample; the 10-bit code sits in the high bits.
        let sample = *(mapped.pData as *const u16);
        ctx.Unmap(&stag, 0);
        sample >> 6
    };
    eprintln!("VideoConverter BGRA(128)->P010 on AMD: Y10={y10} (expect ~504 studio grey)");
    assert!(
        (400..=620).contains(&y10),
        "P010 luma {y10} off BT.709 studio grey — the AMD video processor mis-rendered P010"
    );
}

/// Live intra-refresh property on a scratch component (does not mutate process env).
#[test]
fn amf_intra_refresh_property_live() {
    if let Err(e) = try_factory() {
        eprintln!("skipping: AMF runtime unavailable ({e})");
        return;
    }
    let Some(device) = amd_d3d11_device() else {
        eprintln!("skipping: no AMD adapter on this box");
        return;
    };
    let Ok(lib) = try_factory() else { return };
    let ctx = lib.create_context().expect("AMF CreateContext");
    // SAFETY: `device` is declared first, so it outlives `ctx`.
    assert_eq!(unsafe { ctx.init_dx11(Some(&device)) }, sys::AMF_OK);
    for codec in [Codec::H264, Codec::H265] {
        let props = codec_props(codec);
        let Ok(comp) = lib.create_component(&ctx, props.component) else {
            eprintln!("skipping {codec:?}: component unavailable");
            continue;
        };
        let _ = comp.set_prop(
            props.usage,
            AmfVariant::from_i64(usage_from_knobs(codec)),
            true,
        );
        let (name, block) = props.intra_refresh.expect("AVC/HEVC define intra-refresh");
        let blocks = 640u32.div_ceil(block) * 480u32.div_ceil(block);
        let per_slot = blocks.div_ceil(30).max(1);
        let applied = comp
            .set_prop(name, AmfVariant::from_i64(per_slot as i64), false)
            .expect("optional set_prop never errors");
        eprintln!("intra-refresh {codec:?}: {per_slot} units/slot accepted={applied} on this VCN");
    }
}

/// Burst faster than the encoder drains (no poll between submits). `submit` must drain into
/// `ready` instead of erroring. Asserts IDR-first FIFO across the ready→pending boundary.
#[test]
fn amf_backpressure_burst_live() {
    if let Err(e) = try_factory() {
        eprintln!("skipping: AMF runtime unavailable ({e})");
        return;
    }
    let Some(device) = amd_d3d11_device() else {
        eprintln!("skipping: no AMD adapter on this box");
        return;
    };
    let (w, h, fps) = (640u32, 480u32, 60u32);
    let tex = nv12_texture(&device, w, h, None, BIND_SR);
    let mut enc = match AmfEncoder::open(
        Codec::H265,
        PixelFormat::Nv12,
        w,
        h,
        fps,
        2_000_000,
        8,
        ChromaFormat::Yuv420,
        false,
        None,
    ) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("skipping: native AMF open declined ({e:#})");
            return;
        }
    };
    const BURST: u64 = 48; // >> RING, faster than the ASIC drains
    for i in 1..=BURST {
        let frame = CapturedFrame {
            provenance: Default::default(),
            width: w,
            height: h,
            pts_ns: i,
            format: PixelFormat::Nv12,
            payload: FramePayload::D3d11(pf_frame::dxgi::D3d11Frame {
                texture: tex.clone(),
                device: device.clone(),
                pyro: None,
            }),
            cursor: None,
        };
        // No poll between submits: the in-flight bound must drain, never error.
        enc.submit(&frame)
            .expect("burst submit must ride back-pressure, not error");
    }
    enc.flush().expect("flush");
    let mut aus: Vec<EncodedFrame> = Vec::new();
    for _ in 0..(BURST as usize + 100) {
        match enc.poll().expect("drain poll") {
            Some(au) => aus.push(au),
            None => break,
        }
    }
    assert!(
        aus.len() as u64 >= BURST - 2,
        "most AUs must survive the burst without a reset (got {} of {BURST})",
        aus.len()
    );
    assert!(aus[0].keyframe, "first AU must be the IDR");
    for pair in aus.windows(2) {
        assert!(
            pair[1].pts_ns > pair[0].pts_ns,
            "AUs must stay FIFO-monotonic across the ready→pending boundary: {} then {}",
            pair[0].pts_ns,
            pair[1].pts_ns
        );
    }
    eprintln!(
        "back-pressure burst: {} AUs, FIFO-monotonic, IDR-first — ring bound held, no reset",
        aus.len()
    );
}

/// FFI smoke: load, version-gate, CreateContext + HEVC CreateComponent. A layout error in
/// the mirror crashes; pass/skip is the assertion.
#[test]
fn amf_factory_probe_smoke() {
    let lib = match try_factory() {
        Ok(l) => l,
        Err(e) => {
            eprintln!("skipping: AMF runtime unavailable ({e})");
            return;
        }
    };
    assert!(lib.version >= sys::AMF_MIN_VERSION);
    let ctx = lib.create_context().expect("AMF CreateContext");
    // SAFETY: no device is borrowed: AMF creates and owns its own (fail → skip).
    let r = unsafe { ctx.init_dx11(None) };
    if r != sys::AMF_OK {
        eprintln!(
            "skipping: InitDX11(default device) failed ({})",
            result_name(r)
        );
        return;
    }
    if let Err(e) = lib.create_component(&ctx, h!("AMFVideoEncoderHW_HEVC")) {
        // Probe answer (no HEVC VCN), not a mirror failure.
        eprintln!("note: CreateComponent(HEVC) declined ({e:#})");
    }
}
