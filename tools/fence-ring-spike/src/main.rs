//! `fence-ring-spike` — S2 of the vdisplay stall-immunity plan (Windows).
//!
//! Answers, on real hardware, whether one sealed texture ring plus two shared D3D11 fences can
//! replace the per-slot keyed mutex between the host and a producer process (WP7's transport).
//! One binary, two roles: the parent creates the ring (unnamed shared textures, a shared header
//! section, a producer-ready fence and a consumer-retire fence), spawns itself as `--role
//! producer` with inherited handles, and consumes/verifies; the child opens everything on its own
//! device and produces at a fixed rate. Slot ownership is CAS over `FREE -> WRITING -> PUBLISHED
//! -> READING -> FREE`; the producer never CPU-waits for the reader (a not-yet-retired slot is
//! skipped, GPU-ordered via a device `Wait`), the reader orders its copy after producer-ready.
//!
//! Every frame's sequence number is encoded IN the pixels (clear color) and in the slot header;
//! the consumer maps a staging copy and checks both agree and the image is uniform. Verdicts:
//! torn slots, stale relabels, premature reads, producer CPU-blocks (must all be 0), and death
//! handling (`--kill-after` terminates the producer mid-run; the parent must detect it and end
//! the run instead of waiting forever).
//!
//! Usage: `fence-ring-spike [--adapter <substr>] [--format bgra|fp16] [--rate 120]
//!         [--secs 20] [--reader-stall-ms 0] [--kill-after 0] [--chaos]`
//!
//! Run per adapter on the S2 matrix box (.173 covers NVIDIA + the AMD iGPU). The production
//! transport seals handles through the broker; this spike inherits them — same cross-process
//! open path, simpler plumbing.

#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!("fence-ring-spike is Windows-only (it exercises D3D11 shared fences).");
    std::process::exit(2);
}

#[cfg(target_os = "windows")]
fn main() {
    win::main()
}

#[cfg(target_os = "windows")]
mod win {
    use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    use windows::core::{Interface, PCWSTR};
    use windows::Win32::Foundation::{
        CloseHandle, SetHandleInformation, HANDLE, HANDLE_FLAG_INHERIT, HMODULE,
        INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
    };
    use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
    use windows::Win32::Graphics::Direct3D11::{
        D3D11CreateDevice, ID3D11Device, ID3D11Device1, ID3D11Device5, ID3D11DeviceContext,
        ID3D11DeviceContext4, ID3D11Fence, ID3D11RenderTargetView, ID3D11Texture2D,
        D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_CPU_ACCESS_READ,
        D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_FENCE_FLAG_SHARED, D3D11_MAP_READ,
        D3D11_RESOURCE_MISC_SHARED, D3D11_RESOURCE_MISC_SHARED_NTHANDLE, D3D11_SDK_VERSION,
        D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING,
    };
    use windows::Win32::Graphics::Dxgi::Common::{
        DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_SAMPLE_DESC,
    };
    use windows::Win32::Graphics::Dxgi::{
        CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1, IDXGIResource1,
    };
    use windows::Win32::Security::SECURITY_ATTRIBUTES;
    use windows::Win32::System::Memory::{
        CreateFileMappingW, MapViewOfFile, FILE_MAP_READ, FILE_MAP_WRITE, PAGE_READWRITE,
    };
    use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

    const RING: usize = 6;
    const W: u32 = 1920;
    const H: u32 = 1080;
    const MAGIC: u64 = 0x5f32_5350_494b_4532; // "_2SPIKE2"

    const FREE: u32 = 0;
    const WRITING: u32 = 1;
    const PUBLISHED: u32 = 2;
    const READING: u32 = 3;

    /// Per-slot protocol state in the shared header. `seq` and the fence targets are stamped by
    /// the producer before the `WRITING -> PUBLISHED` release CAS; the consumer stamps
    /// `retire_value` before `READING -> FREE`.
    #[repr(C)]
    struct Slot {
        state: AtomicU32,
        _pad: u32,
        seq: AtomicU64,
        ready_value: AtomicU64,
        retire_value: AtomicU64,
    }

    /// The shared header section. Plain atomics over a mapped view, like the production ring.
    #[repr(C)]
    struct Header {
        magic: AtomicU64,
        generation: AtomicU32,
        producer_alive: AtomicU32,
        produced: AtomicU64,
        skipped_unretired: AtomicU64,
        overwritten_published: AtomicU64,
        slots: [Slot; RING],
    }

    struct Args {
        role_producer: bool,
        adapter: Option<String>,
        format: DXGI_FORMAT,
        rate: u64,
        secs: u64,
        reader_stall_ms: u64,
        kill_after: u64,
        chaos: bool,
        // producer-role plumbing (inherited handle values + adapter LUID)
        handles: Vec<isize>,
        luid: i64,
    }

    fn parse_args() -> Args {
        let mut a = Args {
            role_producer: false,
            adapter: None,
            format: DXGI_FORMAT_B8G8R8A8_UNORM,
            rate: 120,
            secs: 20,
            reader_stall_ms: 0,
            kill_after: 0,
            chaos: false,
            handles: Vec::new(),
            luid: 0,
        };
        let mut it = std::env::args().skip(1);
        while let Some(arg) = it.next() {
            let val = |it: &mut dyn Iterator<Item = String>| {
                it.next().unwrap_or_else(|| {
                    eprintln!("missing value for {arg}");
                    std::process::exit(2);
                })
            };
            match arg.as_str() {
                "--role" => a.role_producer = val(&mut it) == "producer",
                "--adapter" => a.adapter = Some(val(&mut it)),
                "--format" => {
                    a.format = match val(&mut it).as_str() {
                        "fp16" => DXGI_FORMAT_R16G16B16A16_FLOAT,
                        _ => DXGI_FORMAT_B8G8R8A8_UNORM,
                    }
                }
                "--rate" => a.rate = val(&mut it).parse().unwrap_or(120),
                "--secs" => a.secs = val(&mut it).parse().unwrap_or(20),
                "--reader-stall-ms" => a.reader_stall_ms = val(&mut it).parse().unwrap_or(0),
                "--kill-after" => a.kill_after = val(&mut it).parse().unwrap_or(0),
                "--chaos" => a.chaos = true,
                "--handles" => {
                    a.handles = val(&mut it)
                        .split(',')
                        .filter_map(|s| s.parse().ok())
                        .collect()
                }
                "--luid" => a.luid = val(&mut it).parse().unwrap_or(0),
                other => {
                    eprintln!("unknown arg {other}");
                    std::process::exit(2);
                }
            }
        }
        a
    }

    fn pack_luid(l: windows::Win32::Foundation::LUID) -> i64 {
        ((l.HighPart as i64) << 32) | (l.LowPart as i64 & 0xffff_ffff)
    }

    /// Pick the adapter by LUID (producer role) or description substring (parent), else adapter 0.
    fn pick_adapter(sel: &Option<String>, luid: i64) -> (IDXGIAdapter1, i64, String) {
        // SAFETY: factory/adapters/desc are plain out-param COM calls over live locals.
        unsafe {
            let factory: IDXGIFactory1 = CreateDXGIFactory1().expect("CreateDXGIFactory1");
            let mut i = 0;
            let mut fallback: Option<(IDXGIAdapter1, i64, String)> = None;
            while let Ok(ad) = factory.EnumAdapters1(i) {
                i += 1;
                let desc = ad.GetDesc1().expect("GetDesc1");
                let name = String::from_utf16_lossy(&desc.Description)
                    .trim_end_matches('\0')
                    .to_string();
                let l = pack_luid(desc.AdapterLuid);
                if desc.Flags & 0x2 != 0 {
                    continue; // software adapter
                }
                let hit = if luid != 0 {
                    l == luid
                } else if let Some(sub) = sel {
                    name.to_lowercase().contains(&sub.to_lowercase())
                } else {
                    fallback.is_none()
                };
                if hit {
                    return (ad, l, name);
                }
                fallback.get_or_insert((ad, l, name));
            }
            let (ad, l, name) = fallback.expect("no hardware adapter found");
            eprintln!("adapter selector matched nothing — using {name}");
            (ad, l, name)
        }
    }

    fn make_device(adapter: &IDXGIAdapter1) -> (ID3D11Device, ID3D11DeviceContext) {
        let mut dev: Option<ID3D11Device> = None;
        let mut ctx: Option<ID3D11DeviceContext> = None;
        // SAFETY: standard device creation; out-params are live locals, checked below.
        unsafe {
            D3D11CreateDevice(
                adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut dev),
                None,
                Some(&mut ctx),
            )
            .expect("D3D11CreateDevice");
        }
        (dev.expect("device"), ctx.expect("context"))
    }

    /// Inheritable-handle `SECURITY_ATTRIBUTES` — the spike's stand-in for the production broker.
    fn inheritable_sa() -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: std::ptr::null_mut(),
            bInheritHandle: true.into(),
        }
    }

    fn mark_inheritable(h: HANDLE) {
        // SAFETY: `h` is a live handle this process owns; the call only flips its inherit flag.
        unsafe {
            SetHandleInformation(h, HANDLE_FLAG_INHERIT.0, HANDLE_FLAG_INHERIT)
                .expect("SetHandleInformation");
        }
    }

    /// Encode `seq` as a clear color the consumer can decode from mapped pixels: two 8-bit
    /// channels for BGRA (UNORM clamps to 0..1), raw counts for FP16 (halves are exact to 2048).
    fn seq_color(seq: u64, format: DXGI_FORMAT) -> [f32; 4] {
        let lo = (seq & 0xff) as f32;
        let hi = ((seq >> 8) & 0xff) as f32;
        if format == DXGI_FORMAT_R16G16B16A16_FLOAT {
            [lo, hi, 0.5, 1.0]
        } else {
            [lo / 255.0, hi / 255.0, 0.5, 1.0]
        }
    }

    fn decode_seq(px: &[u8], format: DXGI_FORMAT) -> u64 {
        if format == DXGI_FORMAT_R16G16B16A16_FLOAT {
            let h = |o: usize| half_to_f32(u16::from_le_bytes([px[o], px[o + 1]]));
            (h(0).round() as u64 & 0xff) | ((h(2).round() as u64 & 0xff) << 8)
        } else {
            // BGRA byte order: B G R A; the color's R carries lo, G carries hi.
            (px[2] as u64) | ((px[1] as u64) << 8)
        }
    }

    fn half_to_f32(h: u16) -> f32 {
        let (s, e, m) = (
            (h >> 15) as u32,
            ((h >> 10) & 0x1f) as u32,
            (h & 0x3ff) as u32,
        );
        let f = match e {
            0 => (m as f32) * (2f32).powi(-24),
            31 => f32::INFINITY,
            _ => (1.0 + m as f32 / 1024.0) * (2f32).powi(e as i32 - 15),
        };
        if s == 1 {
            -f
        } else {
            f
        }
    }

    struct Shared {
        header: *mut Header,
        textures: Vec<ID3D11Texture2D>,
        ready: ID3D11Fence,
        retire: ID3D11Fence,
    }

    fn header(sh: &Shared) -> &Header {
        // SAFETY: the mapping outlives the process phase using it and is Header-sized+aligned by
        // construction (page-aligned view, #[repr(C)] atomics).
        unsafe { &*sh.header }
    }

    pub fn main() {
        let a = parse_args();
        if a.role_producer {
            producer(a);
        } else {
            parent(a);
        }
    }

    // ---- parent: create the ring, spawn the producer, consume + verify ----------------------

    fn parent(a: Args) {
        let (adapter, luid, name) = pick_adapter(&a.adapter, 0);
        println!(
            "adapter: {name} (luid {luid:#x}) format={} rate={} secs={} reader_stall_ms={} kill_after={} chaos={}",
            if a.format == DXGI_FORMAT_R16G16B16A16_FLOAT { "fp16" } else { "bgra" },
            a.rate, a.secs, a.reader_stall_ms, a.kill_after, a.chaos
        );
        let (dev, ctx) = make_device(&adapter);
        let dev5: ID3D11Device5 = dev.cast().expect("ID3D11Device5 (fences need 11.4)");
        let ctx4: ID3D11DeviceContext4 = ctx.cast().expect("ID3D11DeviceContext4");

        let sa = inheritable_sa();
        // Header section.
        let map_size = std::mem::size_of::<Header>();
        // SAFETY: unnamed pagefile-backed mapping with a live SA; the view is checked non-null.
        let (map, header_ptr) = unsafe {
            let map = CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                Some(&sa),
                PAGE_READWRITE,
                0,
                map_size as u32,
                PCWSTR::null(),
            )
            .expect("CreateFileMappingW");
            let view = MapViewOfFile(map, FILE_MAP_READ | FILE_MAP_WRITE, 0, 0, map_size);
            assert!(!view.Value.is_null(), "MapViewOfFile");
            std::ptr::write_bytes(view.Value.cast::<u8>(), 0, map_size);
            (map, view.Value.cast::<Header>())
        };

        // Ring textures: shared NTHANDLE, NO keyed mutex — that is the experiment.
        let mut textures = Vec::new();
        let mut tex_handles = Vec::new();
        for _ in 0..RING {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: W,
                Height: H,
                MipLevels: 1,
                ArraySize: 1,
                Format: a.format,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
                CPUAccessFlags: 0,
                // NTHANDLE sharing must pair with SHARED (or KEYEDMUTEX — the thing this spike
                // removes); NTHANDLE alone is E_INVALIDARG.
                MiscFlags: (D3D11_RESOURCE_MISC_SHARED.0 | D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0)
                    as u32,
            };
            let mut tex: Option<ID3D11Texture2D> = None;
            // SAFETY: checked out-param creation over a fully-initialised descriptor; the shared
            // handle is minted for this texture and marked inheritable (the spike's broker).
            unsafe {
                dev.CreateTexture2D(&desc, None, Some(&mut tex))
                    .expect("CreateTexture2D");
                let tex = tex.expect("texture");
                let res1: IDXGIResource1 = tex.cast().expect("IDXGIResource1");
                // DXGI_SHARED_RESOURCE_READ | WRITE — the `windows` crate does not export the
                // combined u32 (same local as the production ring keeps).
                let h = res1
                    .CreateSharedHandle(Some(&sa), 0x8000_0000 | 0x1, PCWSTR::null())
                    .expect("CreateSharedHandle(texture)");
                mark_inheritable(h);
                tex_handles.push(h.0 as isize);
                textures.push(tex);
            }
        }

        // The two shared fences.
        let mk_fence = || {
            let mut f: Option<ID3D11Fence> = None;
            // SAFETY: checked out-param creation; the shared handle is minted for this fence.
            unsafe {
                dev5.CreateFence(0, D3D11_FENCE_FLAG_SHARED, &mut f)
                    .expect("CreateFence");
                let f = f.expect("fence");
                let h = f
                    .CreateSharedHandle(Some(&sa), 0x1000_0000, PCWSTR::null())
                    .expect("CreateSharedHandle(fence)");
                mark_inheritable(h);
                (f, h.0 as isize)
            }
        };
        let (ready, ready_h) = mk_fence();
        let (retire, retire_h) = mk_fence();
        mark_inheritable(map);

        let sh = Shared {
            header: header_ptr,
            textures,
            ready,
            retire,
        };
        let hd = header(&sh);
        hd.magic.store(MAGIC, Ordering::Release);
        hd.generation.store(1, Ordering::Release);
        hd.producer_alive.store(1, Ordering::Release);

        // Spawn the producer with the inherited handle values on its command line.
        let mut handles = vec![map.0 as isize];
        handles.push(ready_h);
        handles.push(retire_h);
        handles.extend(&tex_handles);
        let handle_list = handles
            .iter()
            .map(|h| h.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let exe = std::env::current_exe().expect("current_exe");
        let mut child = std::process::Command::new(exe)
            .args([
                "--role",
                "producer",
                "--handles",
                &handle_list,
                "--luid",
                &luid.to_string(),
            ])
            .args([
                "--rate",
                &a.rate.to_string(),
                "--secs",
                &(a.secs + 5).to_string(),
            ])
            .args([
                "--format",
                if a.format == DXGI_FORMAT_R16G16B16A16_FLOAT {
                    "fp16"
                } else {
                    "bgra"
                },
            ])
            .args(if a.chaos { vec!["--chaos"] } else { vec![] })
            .spawn()
            .expect("spawn producer");

        consume_and_verify(&a, &sh, &dev, &ctx, &ctx4, &mut child);
    }

    /// The consumer/verifier loop plus the run verdict — the parent's whole purpose.
    #[allow(clippy::too_many_lines)]
    fn consume_and_verify(
        a: &Args,
        sh: &Shared,
        dev: &ID3D11Device,
        ctx: &ID3D11DeviceContext,
        ctx4: &ID3D11DeviceContext4,
        child: &mut std::process::Child,
    ) {
        let hd = header(sh);
        // Staging target for readback.
        let sdesc = D3D11_TEXTURE2D_DESC {
            Width: W,
            Height: H,
            MipLevels: 1,
            ArraySize: 1,
            Format: a.format,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut staging: Option<ID3D11Texture2D> = None;
        // SAFETY: checked out-param creation over a fully-initialised descriptor.
        unsafe {
            dev.CreateTexture2D(&sdesc, None, Some(&mut staging))
                .expect("staging")
        };
        let staging = staging.expect("staging");
        // SAFETY: unnamed auto-reset event, used only with SetEventOnCompletion + a bounded wait.
        let event = unsafe { CreateEventW(None, false, false, PCWSTR::null()).expect("event") };

        let deadline = Instant::now() + Duration::from_secs(a.secs);
        let mut retire_v: u64 = 0;
        let (mut consumed, mut torn, mut stale, mut premature, mut last_seq, mut stale_dropped) =
            (0u64, 0u64, 0u64, 0u64, 0u64, 0u64);
        let mut kill_done = a.kill_after == 0;
        let mut death_at: Option<Instant> = None;
        let bpp = if a.format == DXGI_FORMAT_R16G16B16A16_FLOAT {
            8
        } else {
            4
        };

        loop {
            if Instant::now() >= deadline {
                break;
            }
            if !kill_done && consumed >= a.kill_after {
                println!("killing producer after {consumed} consumed frames");
                let _ = child.kill();
                kill_done = true;
            }
            if death_at.is_none() {
                if let Ok(Some(st)) = child.try_wait() {
                    death_at = Some(Instant::now());
                    println!("producer exited ({st}) — watching for permanent-wait symptoms");
                }
            }
            // Newest PUBLISHED slot by seq; CAS it to READING (a lost race just rescans).
            let mut pick: Option<(usize, u64)> = None;
            for (i, s) in hd.slots.iter().enumerate() {
                if s.state.load(Ordering::Acquire) == PUBLISHED {
                    let seq = s.seq.load(Ordering::Acquire);
                    if seq <= last_seq {
                        // Older than what was already delivered: newest-wins DROPS it (a stream
                        // must never show it) — S2 finding, first matrix run under --chaos.
                        if s.state
                            .compare_exchange(PUBLISHED, FREE, Ordering::AcqRel, Ordering::Acquire)
                            .is_ok()
                        {
                            stale_dropped += 1;
                        }
                        continue;
                    }
                    if pick.is_none_or(|(_, ps)| seq > ps) {
                        pick = Some((i, seq));
                    }
                }
            }
            let Some((i, seq)) = pick else {
                if death_at.is_some_and(|t| t.elapsed() > Duration::from_secs(2)) {
                    println!("ring drained after producer death — no permanent wait (PASS leg)");
                    break;
                }
                std::thread::sleep(Duration::from_millis(2));
                continue;
            };
            let slot = &hd.slots[i];
            if slot
                .state
                .compare_exchange(PUBLISHED, READING, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                continue; // producer overwrote first — the designed race, rescan
            }
            let ready_v = slot.ready_value.load(Ordering::Acquire);
            // Order the copy after producer-ready on the GPU timeline, then fence OUR copy so the
            // map below provably reads completed pixels.
            retire_v += 1;
            // SAFETY: GPU calls over live COM objects on this thread's immediate context; the
            // bounded event wait covers the copy's completion via the retire signal queued after.
            unsafe {
                ctx4.Wait(&sh.ready, ready_v).expect("ctx.Wait(ready)");
                ctx.CopyResource(&staging, &sh.textures[i]);
                ctx4.Signal(&sh.retire, retire_v)
                    .expect("ctx.Signal(retire)");
                slot.retire_value.store(retire_v, Ordering::Release);
                sh.retire
                    .SetEventOnCompletion(retire_v, event)
                    .expect("SetEventOnCompletion");
                if WaitForSingleObject(event, 2000) != WAIT_OBJECT_0 {
                    premature += 1;
                    println!("retire fence wait timed out — GPU stalled? (counted premature)");
                }
            }
            if a.reader_stall_ms > 0 && consumed % 60 == 0 {
                std::thread::sleep(Duration::from_millis(a.reader_stall_ms));
            }
            // Map + verify: all sample points must decode the SAME seq, and match the header's.
            // SAFETY: Map/Unmap pair on the staging texture; row pointers stay inside the mapped
            // extent (pitch-bounded indexing below).
            unsafe {
                let mut mapped = Default::default();
                ctx.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                    .expect("Map");
                let base = mapped.pData.cast::<u8>();
                let pitch = mapped.RowPitch as usize;
                let sample = |x: u32, y: u32| {
                    let p = base.add(y as usize * pitch + x as usize * bpp);
                    decode_seq(std::slice::from_raw_parts(p, bpp), a.format)
                };
                let pts = [
                    sample(0, 0),
                    sample(W - 1, 0),
                    sample(0, H - 1),
                    sample(W - 1, H - 1),
                    sample(W / 2, H / 2),
                ];
                ctx.Unmap(&staging, 0);
                let uniform = pts.iter().all(|&p| p == pts[0]);
                if !uniform {
                    torn += 1;
                    println!("TORN slot {i}: samples {pts:?} (header seq {seq})");
                } else if pts[0] != (seq & 0xffff) {
                    stale += 1;
                    println!(
                        "STALE RELABEL slot {i}: pixels say {} header says {seq}",
                        pts[0]
                    );
                }
            }
            if seq <= last_seq {
                println!("non-monotonic consume: {seq} after {last_seq} (newest-wins scan bug?)");
            }
            last_seq = seq;
            consumed += 1;
            slot.state.store(FREE, Ordering::Release);
        }

        let produced = hd.produced.load(Ordering::Acquire);
        let skipped = hd.skipped_unretired.load(Ordering::Acquire);
        let over = hd.overwritten_published.load(Ordering::Acquire);
        let _ = child.kill();
        let _ = child.wait();
        // SAFETY: sole owner of the event handle; closed once at the end of the run.
        unsafe {
            let _ = CloseHandle(event);
        }
        println!(
            "VERDICT consumed={consumed} produced={produced} torn={torn} stale_relabel={stale} \
             premature={premature} stale_dropped={stale_dropped} \
             producer_skipped_unretired={skipped} producer_overwrote_published={over} \
             last_seq={last_seq}"
        );
        let pass = torn == 0 && stale == 0 && premature == 0 && consumed > 0;
        println!("{}", if pass { "PASS" } else { "FAIL" });
        std::process::exit(if pass { 0 } else { 1 });
    }

    // ---- producer (child): open everything, produce at rate, never CPU-block on the reader ----

    fn producer(a: Args) {
        assert!(a.handles.len() == 3 + RING, "expected {} handles", 3 + RING);
        let (map_h, ready_h, retire_h) = (a.handles[0], a.handles[1], a.handles[2]);
        let (adapter, _luid, name) = pick_adapter(&None, a.luid);
        let (dev, ctx) = make_device(&adapter);
        let dev1: ID3D11Device1 = dev.cast().expect("ID3D11Device1");
        let dev5: ID3D11Device5 = dev.cast().expect("ID3D11Device5");
        let ctx4: ID3D11DeviceContext4 = ctx.cast().expect("ID3D11DeviceContext4");
        eprintln!("producer: on {name}, opening inherited ring");

        // SAFETY: the handle values were inherited from the parent (live in this process); the
        // view is checked non-null and the magic is verified before any slot is touched.
        let header_ptr = unsafe {
            let view = MapViewOfFile(
                HANDLE(map_h as *mut core::ffi::c_void),
                FILE_MAP_READ | FILE_MAP_WRITE,
                0,
                0,
                std::mem::size_of::<Header>(),
            );
            assert!(!view.Value.is_null(), "MapViewOfFile(inherited)");
            view.Value.cast::<Header>()
        };
        let mut textures = Vec::new();
        let mut rtvs: Vec<ID3D11RenderTargetView> = Vec::new();
        // SAFETY: each inherited value names a live shared-texture handle; opens are checked, and
        // the RTVs are created on this process's own device.
        unsafe {
            for &th in &a.handles[3..] {
                let tex: ID3D11Texture2D = dev1
                    .OpenSharedResource1(HANDLE(th as *mut core::ffi::c_void))
                    .expect("OpenSharedResource1");
                let mut rtv: Option<ID3D11RenderTargetView> = None;
                dev.CreateRenderTargetView(&tex, None, Some(&mut rtv))
                    .expect("CreateRenderTargetView");
                rtvs.push(rtv.expect("rtv"));
                textures.push(tex);
            }
        }
        let open_fence = |h: isize| {
            let mut f: Option<ID3D11Fence> = None;
            // SAFETY: the inherited value names a live shared-fence handle; the open is checked.
            unsafe {
                dev5.OpenSharedFence(HANDLE(h as *mut core::ffi::c_void), &mut f)
                    .expect("OpenSharedFence");
            }
            f.expect("opened fence")
        };
        let ready = open_fence(ready_h);
        let retire = open_fence(retire_h);
        let sh = Shared {
            header: header_ptr,
            textures,
            ready,
            retire,
        };
        let hd = header(&sh);
        assert_eq!(hd.magic.load(Ordering::Acquire), MAGIC, "bad header magic");

        let interval = Duration::from_nanos(1_000_000_000 / a.rate.max(1));
        let deadline = Instant::now() + Duration::from_secs(a.secs);
        let (mut seq, mut ready_v) = (0u64, 0u64);
        let mut next = Instant::now();
        while Instant::now() < deadline {
            next += interval;
            seq += 1;
            // Claim: a FREE slot first; else the OLDEST PUBLISHED (overwrite unconsumed — the
            // drop-oldest contract); NEVER a READING or WRITING slot. No CPU wait anywhere.
            let mut claim: Option<usize> = None;
            let mut oldest: Option<(usize, u64)> = None;
            for (i, s) in hd.slots.iter().enumerate() {
                match s.state.load(Ordering::Acquire) {
                    FREE => {
                        // A slot the reader retired must be GPU-waited to its retire value before
                        // the copy — queued on OUR timeline, never a CPU block.
                        if s.state
                            .compare_exchange(FREE, WRITING, Ordering::AcqRel, Ordering::Acquire)
                            .is_ok()
                        {
                            claim = Some(i);
                            break;
                        }
                    }
                    PUBLISHED => {
                        let sq = s.seq.load(Ordering::Acquire);
                        if oldest.is_none_or(|(_, os)| sq < os) {
                            oldest = Some((i, sq));
                        }
                    }
                    _ => {}
                }
            }
            let i = match claim {
                Some(i) => i,
                None => match oldest {
                    Some((i, _))
                        if hd.slots[i]
                            .state
                            .compare_exchange(
                                PUBLISHED,
                                WRITING,
                                Ordering::AcqRel,
                                Ordering::Acquire,
                            )
                            .is_ok() =>
                    {
                        hd.overwritten_published.fetch_add(1, Ordering::AcqRel);
                        i
                    }
                    _ => {
                        // Everything is READING/WRITING/contended — drop the frame, never wait.
                        hd.skipped_unretired.fetch_add(1, Ordering::AcqRel);
                        pace(&mut next, interval, a.chaos);
                        continue;
                    }
                },
            };
            let slot = &hd.slots[i];
            let color = seq_color(seq, a.format);
            // SAFETY: GPU calls over live COM objects; the retire Wait is GPU-queued (the
            // never-CPU-block rule), and Signal orders the clear before the consumer's copy.
            unsafe {
                ctx4.Wait(&sh.retire, slot.retire_value.load(Ordering::Acquire))
                    .expect("ctx.Wait(retire)");
                ctx.ClearRenderTargetView(&rtvs[i], &color);
                ready_v += 1;
                ctx4.Signal(&sh.ready, ready_v).expect("ctx.Signal(ready)");
            }
            slot.seq.store(seq, Ordering::Release);
            slot.ready_value.store(ready_v, Ordering::Release);
            slot.state.store(PUBLISHED, Ordering::Release);
            hd.produced.fetch_add(1, Ordering::AcqRel);
            pace(&mut next, interval, a.chaos);
        }
        hd.producer_alive.store(0, Ordering::Release);
        drop(ctx4);
    }

    /// Sleep to the grid instant; `chaos` adds a random 0-8 ms jitter (a cheap PRNG off the
    /// clock) so the two processes' claims interleave differently every run.
    fn pace(next: &mut Instant, interval: Duration, chaos: bool) {
        let mut target = *next;
        if chaos {
            // The wall clock's nanos, not an `Instant` read back at once (tens of ns, always).
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.subsec_nanos());
            let j = u64::from(nanos % 8_000_000);
            target += Duration::from_nanos(j);
        }
        let now = Instant::now();
        if target > now {
            std::thread::sleep(target - now);
        } else {
            *next = now; // fell behind — re-anchor instead of bursting
        }
        let _ = interval;
    }
}
