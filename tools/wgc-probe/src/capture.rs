//! The WGC side: name the output, open the capture in timed stages, drain frames, measure.
//!
//! The device is plain: no GPU scheduling class, no thread priority. That is what a viewer's
//! worker gets, so `--touch` costs the owner what a viewer would.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use windows::core::{h, w, IInspectable, Interface, HSTRING};
use windows::Foundation::Metadata::ApiInformation;
use windows::Foundation::{TimeSpan, TypedEventHandler};
use windows::Graphics::Capture::{
    Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureAccess,
    GraphicsCaptureAccessKind, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::{COLORREF, HMODULE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
    D3D11_BIND_SHADER_RESOURCE, D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dwm::{DwmFlush, DwmGetCompositionTimingInfo, DWM_TIMING_INFO};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020, DXGI_FORMAT_R16G16B16A16_FLOAT,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter1, IDXGIDevice, IDXGIFactory1, IDXGIOutput6,
};
use windows::Win32::Graphics::Gdi::{
    CreateSolidBrush, DeleteObject, FillRect, GetDC, ReleaseDC, HMONITOR,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, OpenInputDesktop, DESKTOP_ACCESS_FLAGS, DESKTOP_CONTROL_FLAGS,
};
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_MOVE, MOUSEINPUT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetCursorInfo, PeekMessageW,
    RegisterClassW, ShowWindow, CURSORINFO, MSG, PM_REMOVE, SW_SHOWNOACTIVATE, WNDCLASSW,
    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

use crate::win::{hr, log, Opts};

/// A stage that runs longer than this is a hang; the design's bound on `OPEN_SOURCE`.
const HANG: Duration = Duration::from_secs(5);
/// How long a capture waits for its first frame before it reports none.
const FIRST_FRAME: Duration = Duration::from_secs(5);
/// How long after a canary kick frames are collected. The canary shows for 250 ms, so the last
/// frame inside this window is the settled picture.
const SETTLE: Duration = Duration::from_millis(450);
const SESSION_CLASS: &HSTRING = h!("Windows.Graphics.Capture.GraphicsCaptureSession");

/// `(x, y, w, h)` in desktop pixels.
type Rect = (i32, i32, i32, i32);

struct Target {
    gdi: String,
    adapter: IDXGIAdapter1,
    adapter_name: String,
    luid: i64,
    monitor: HMONITOR,
    rect: Rect,
    rotation: i32,
    colorspace: i32,
    hdr: bool,
}

impl std::fmt::Display for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (x, y, w, h) = self.rect;
        write!(
            f,
            "gdi={:?} adapter={:?} luid={:#x} rect={x},{y},{w}x{h} rotation={} colorspace={} hdr={}",
            self.gdi, self.adapter_name, self.luid, self.rotation, self.colorspace, self.hdr
        )
    }
}

fn wide(s: &[u16]) -> String {
    let end = s.iter().position(|&c| c == 0).unwrap_or(s.len());
    String::from_utf16_lossy(&s[..end])
}

/// WinRT on this thread, and real pixels from every coordinate call.
fn init() {
    // SAFETY: both calls take scalars and only report through their return value. A second
    // apartment init or an already-set awareness is harmless.
    unsafe {
        let _ = RoInitialize(RO_INIT_MULTITHREADED);
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

/// Every output DXGI enumerates, under the adapter that enumerates it.
fn targets() -> Result<Vec<Target>, String> {
    let mut all = Vec::new();
    // SAFETY: plain DXGI enumeration; each interface is the checked return of the call before.
    unsafe {
        let factory: IDXGIFactory1 = hr(CreateDXGIFactory1(), "CreateDXGIFactory1")?;
        let mut a = 0;
        while let Ok(adapter) = factory.EnumAdapters1(a) {
            a += 1;
            let ad = hr(adapter.GetDesc1(), "adapter GetDesc1")?;
            let mut o = 0;
            while let Ok(output) = adapter.EnumOutputs(o) {
                o += 1;
                let d = hr(output.GetDesc(), "output GetDesc")?;
                let colorspace = output
                    .cast::<IDXGIOutput6>()
                    .ok()
                    .and_then(|o6| o6.GetDesc1().ok())
                    .map_or(-1, |d1| d1.ColorSpace.0);
                let r = d.DesktopCoordinates;
                all.push(Target {
                    gdi: wide(&d.DeviceName),
                    adapter: adapter.clone(),
                    adapter_name: wide(&ad.Description),
                    luid: (i64::from(ad.AdapterLuid.HighPart) << 32)
                        | i64::from(ad.AdapterLuid.LowPart),
                    monitor: d.Monitor,
                    rect: (r.left, r.top, r.right - r.left, r.bottom - r.top),
                    rotation: d.Rotation.0,
                    colorspace,
                    hdr: colorspace == DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020.0,
                });
            }
        }
    }
    Ok(all)
}

/// `ours` and `physical` pick the first active head of that kind from the host's inventory;
/// anything else is a GDI name.
fn resolve(want: &str) -> Result<Target, String> {
    let gdi = match want {
        "ours" | "physical" => {
            let snap = pf_win_display::display_events::snapshot_or_query();
            snap.targets
                .iter()
                .find(|t| t.active && t.ours == (want == "ours"))
                .map(|t| t.gdi_name.clone())
                .ok_or(format!("no active {want} monitor in the display inventory"))?
        }
        name => name.to_string(),
    };
    let all = targets()?;
    let names: Vec<String> = all.iter().map(|t| t.gdi.clone()).collect();
    all.into_iter()
        .find(|t| t.gdi.eq_ignore_ascii_case(&gdi))
        .ok_or(format!("no DXGI output named {gdi:?}; have {names:?}"))
}

pub fn list() -> Result<i32, String> {
    init();
    let snap = pf_win_display::display_events::snapshot_or_query();
    for t in targets()? {
        let inv = snap
            .targets
            .iter()
            .find(|i| i.gdi_name.eq_ignore_ascii_case(&t.gdi));
        let host = inv.map_or("inventory=absent".to_string(), |i| {
            format!(
                "ours={} friendly={:?} refresh_mhz={} inv_hdr={:?} primary={} tech={}",
                i.ours, i.friendly, i.refresh_mhz, i.hdr, i.primary, i.tech
            )
        });
        log("output", format!("{t} {host}"));
    }
    for i in snap.targets.iter().filter(|i| !i.active) {
        log(
            "inactive",
            format!(
                "target={} friendly={:?} ours={}",
                i.target_id, i.friendly, i.ours
            ),
        );
    }
    let has = |p: &str| ApiInformation::IsPropertyPresent(SESSION_CLASS, &HSTRING::from(p));
    let contract = |major: u16| {
        ApiInformation::IsApiContractPresentByMajor(
            h!("Windows.Foundation.UniversalApiContract"),
            major,
        )
        .unwrap_or(false)
    };
    log("dwm", dwm_timing());
    log(
        "api",
        format!(
            "supported={:?} border={:?} cursor={:?} min_update_interval={:?} dirty_region={:?} \
             contract12={} contract15={} contract19={}",
            GraphicsCaptureSession::IsSupported(),
            has("IsBorderRequired"),
            has("IsCursorCaptureEnabled"),
            has("MinUpdateInterval"),
            has("DirtyRegionMode"),
            contract(12),
            contract(15),
            contract(19),
        ),
    );
    Ok(0)
}

/// Ends the process when a stage outlives [`HANG`]: a hung WGC call cannot be interrupted, and
/// the count of such exits over many opens is the measurement.
#[derive(Clone)]
struct Watchdog(Arc<Mutex<Option<(&'static str, Instant)>>>);

impl Watchdog {
    fn start() -> Self {
        let wd = Self(Arc::new(Mutex::new(None)));
        let seen = wd.clone();
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_millis(250));
            let running = *seen.0.lock().unwrap();
            if let Some((stage, since)) = running.filter(|(_, since)| since.elapsed() > HANG) {
                log(
                    "hang",
                    format!("stage={stage} after_ms={}", since.elapsed().as_millis()),
                );
                std::process::exit(3);
            }
        });
        wd
    }

    fn stage<T>(&self, name: &'static str, f: impl FnOnce() -> T) -> (T, Duration) {
        let t = Instant::now();
        *self.0.lock().unwrap() = Some((name, t));
        let r = f();
        *self.0.lock().unwrap() = None;
        (r, t.elapsed())
    }
}

/// Arrivals counted by the pool's own thread; the main thread takes exactly that many.
struct Signal {
    arrived: AtomicU64,
    lock: Mutex<()>,
    cv: Condvar,
}

struct Session {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    d3d: IDirect3DDevice,
    _item: GraphicsCaptureItem,
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    signal: Arc<Signal>,
    closed: Arc<AtomicBool>,
    /// `TryGetNextFrame` calls made, against `signal.arrived`.
    taken: u64,
    /// Calls that returned no frame: the pool had already recycled it.
    missed: u64,
    size: SizeInt32,
    format: DirectXPixelFormat,
    buffers: i32,
    stages: Vec<(&'static str, Duration)>,
    /// When `StartCapture` returned.
    started: Instant,
    staging: Option<(ID3D11Texture2D, D3D11_TEXTURE2D_DESC)>,
    scratch: Option<(ID3D11Texture2D, D3D11_TEXTURE2D_DESC)>,
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}

fn make_device(adapter: &IDXGIAdapter1) -> Result<(ID3D11Device, ID3D11DeviceContext), String> {
    let (mut device, mut context) = (None, None);
    // SAFETY: `adapter` is live for the call; the out-params are local `Option`s checked below.
    let made = unsafe {
        D3D11CreateDevice(
            adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
    };
    hr(made, "D3D11CreateDevice")?;
    Ok((device.ok_or("null device")?, context.ok_or("null context")?))
}

fn wrap(device: &ID3D11Device) -> Result<IDirect3DDevice, String> {
    let dxgi: IDXGIDevice = hr(device.cast(), "ID3D11Device as IDXGIDevice")?;
    // SAFETY: `dxgi` is a live interface; the call returns an owned, checked object.
    let inspectable = hr(
        unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi) },
        "CreateDirect3D11DeviceFromDXGIDevice",
    )?;
    hr(inspectable.cast(), "IInspectable as IDirect3DDevice")
}

fn create_item(monitor: HMONITOR) -> Result<GraphicsCaptureItem, String> {
    let interop = hr(
        windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>(),
        "GraphicsCaptureItem factory",
    )?;
    // SAFETY: `monitor` came from this process's own DXGI enumeration a moment ago.
    hr(
        unsafe { interop.CreateForMonitor(monitor) },
        "CreateForMonitor",
    )
}

fn texture(frame: &Direct3D11CaptureFrame) -> Result<ID3D11Texture2D, String> {
    let surface = hr(frame.Surface(), "frame Surface")?;
    let access: IDirect3DDxgiInterfaceAccess = hr(surface.cast(), "IDirect3DDxgiInterfaceAccess")?;
    // SAFETY: `access` is live; the call returns an owned, checked interface.
    hr(
        unsafe { access.GetInterface() },
        "GetInterface(ID3D11Texture2D)",
    )
}

/// QPC now, in the 100 ns units `SystemRelativeTime` uses.
fn qpc_100ns() -> i64 {
    let (mut count, mut freq) = (0i64, 0i64);
    // SAFETY: both out-params are live locals; neither call fails on a supported Windows.
    unsafe {
        let _ = QueryPerformanceCounter(&mut count);
        let _ = QueryPerformanceFrequency(&mut freq);
    }
    (i128::from(count) * 10_000_000 / i128::from(freq.max(1))) as i64
}

/// Whether this process can open the input desktop. A user process cannot while Winlogon owns
/// input, so `false` means a prompt, the lock screen or the sign-in screen is up.
fn input_desktop_open() -> bool {
    const DESKTOP_READOBJECTS: u32 = 0x0001;
    // SAFETY: an owned handle only on `Ok`, closed once and not used after.
    unsafe {
        OpenInputDesktop(
            DESKTOP_CONTROL_FLAGS(0),
            false,
            DESKTOP_ACCESS_FLAGS(DESKTOP_READOBJECTS),
        )
        .map(|h| {
            let _ = CloseDesktop(h);
        })
        .is_ok()
    }
}

impl Session {
    fn open(t: &Target, o: &Opts, wd: &Watchdog) -> Result<Self, String> {
        let mut stages = Vec::new();
        let mut run = |name: &'static str, d: Duration| stages.push((name, d));

        let (made, d) = wd.stage("device", || make_device(&t.adapter));
        run("device", d);
        let (device, context) = made?;
        let (d3d, d) = wd.stage("wrap", || wrap(&device));
        run("wrap", d);
        let d3d = d3d?;
        let (item, d) = wd.stage("item", || create_item(t.monitor));
        run("item", d);
        let item = item?;
        let size = hr(item.Size(), "item Size")?;
        let format = if o.fp16 {
            DirectXPixelFormat::R16G16B16A16Float
        } else {
            DirectXPixelFormat::B8G8R8A8UIntNormalized
        };
        let (pool, d) = wd.stage("pool", || {
            Direct3D11CaptureFramePool::CreateFreeThreaded(&d3d, format, o.buffers, size)
        });
        run("pool", d);
        let pool = hr(pool, "CreateFreeThreaded")?;

        let signal = Arc::new(Signal {
            arrived: AtomicU64::new(0),
            lock: Mutex::new(()),
            cv: Condvar::new(),
        });
        let sig = signal.clone();
        let on_frame =
            TypedEventHandler::<Direct3D11CaptureFramePool, IInspectable>::new(move |_, _| {
                sig.arrived.fetch_add(1, Ordering::Release);
                sig.cv.notify_one();
                Ok(())
            });
        hr(pool.FrameArrived(&on_frame), "FrameArrived")?;
        let closed = Arc::new(AtomicBool::new(false));
        let flag = closed.clone();
        let on_closed = TypedEventHandler::<GraphicsCaptureItem, IInspectable>::new(move |_, _| {
            flag.store(true, Ordering::Release);
            Ok(())
        });
        hr(item.Closed(&on_closed), "item Closed")?;

        let (session, d) = wd.stage("session", || pool.CreateCaptureSession(&item));
        run("session", d);
        let session = hr(session, "CreateCaptureSession")?;

        let has = |p: &str| {
            ApiInformation::IsPropertyPresent(SESSION_CLASS, &HSTRING::from(p)).unwrap_or(false)
        };
        if o.access {
            let (status, d) = wd.stage("access", || {
                GraphicsCaptureAccess::RequestAccessAsync(GraphicsCaptureAccessKind::Borderless)
                    .and_then(|op| op.join())
            });
            run("access", d);
            log(
                "access",
                match status {
                    Ok(s) => format!("status={} (4=allowed)", s.0),
                    Err(e) => format!("error={:#010x}", e.code().0 as u32),
                },
            );
        }
        if !o.keep_border {
            let set = session.SetIsBorderRequired(false);
            log(
                "border",
                format!(
                    "present={} set={:?} reads_back_required={:?}",
                    has("IsBorderRequired"),
                    set.map_err(|e| format!("{:#010x}", e.code().0 as u32)),
                    session.IsBorderRequired().ok()
                ),
            );
        }
        hr(
            session.SetIsCursorCaptureEnabled(o.cursor),
            "SetIsCursorCaptureEnabled",
        )?;
        if o.fps > 0 && has("MinUpdateInterval") {
            let ticks = i64::from(10_000_000 / o.fps.max(1)).max(1);
            let set = session.SetMinUpdateInterval(TimeSpan { Duration: ticks });
            log(
                "interval",
                format!(
                    "asked_100ns={ticks} set={:?} reads_back={:?}",
                    set.map_err(|e| format!("{:#010x}", e.code().0 as u32)),
                    session.MinUpdateInterval().map(|t| t.Duration).ok()
                ),
            );
        } else {
            log(
                "interval",
                format!("present={} asked_fps={}", has("MinUpdateInterval"), o.fps),
            );
        }
        let (start, d) = wd.stage("start", || session.StartCapture());
        run("start", d);
        hr(start, "StartCapture")?;

        Ok(Self {
            device,
            context,
            d3d,
            _item: item,
            pool,
            session,
            signal,
            closed,
            taken: 0,
            missed: 0,
            size,
            format,
            buffers: o.buffers,
            stages,
            started: Instant::now(),
            staging: None,
            scratch: None,
        })
    }

    fn stages(&self) -> String {
        self.stages
            .iter()
            .map(|(name, d)| format!("{name}_us={}", d.as_micros()))
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Wait up to `timeout` for an arrival, then take every frame that arrived. Newest last.
    fn take(&mut self, timeout: Duration) -> Vec<Direct3D11CaptureFrame> {
        {
            let guard = self.signal.lock.lock().unwrap();
            let _ = self
                .signal
                .cv
                .wait_timeout_while(guard, timeout, |()| {
                    self.signal.arrived.load(Ordering::Acquire) <= self.taken
                })
                .unwrap();
        }
        let arrived = self.signal.arrived.load(Ordering::Acquire);
        let mut frames = Vec::new();
        while self.taken < arrived {
            self.taken += 1;
            match self.pool.TryGetNextFrame() {
                Ok(f) => frames.push(f),
                Err(_) => self.missed += 1,
            }
        }
        frames
    }

    /// Kick the canary and return the settled frame that follows, closing the rest.
    fn grab(&mut self, rect: Rect) -> Result<Direct3D11CaptureFrame, String> {
        // Whatever arrived before the kick is not the picture being asked for.
        for stale in self.take(Duration::ZERO) {
            let _ = stale.Close();
        }
        if !pf_win_display::compose_probe::present(rect) {
            return Err("the compose canary has no window".into());
        }
        let until = Instant::now() + SETTLE;
        let mut last: Option<Direct3D11CaptureFrame> = None;
        while Instant::now() < until {
            for f in self.take(Duration::from_millis(50)) {
                if let Some(old) = last.replace(f) {
                    let _ = old.Close();
                }
            }
        }
        last.ok_or("no frame followed a canary kick".into())
    }

    /// A texture shaped like `tex` for `usage`, kept until the source's shape changes.
    fn like(
        device: &ID3D11Device,
        slot: &mut Option<(ID3D11Texture2D, D3D11_TEXTURE2D_DESC)>,
        tex: &ID3D11Texture2D,
        staging: bool,
    ) -> Result<ID3D11Texture2D, String> {
        let mut src = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: `tex` is live and `src` is a live out-param.
        unsafe { tex.GetDesc(&mut src) };
        if let Some((have, d)) = slot.as_ref() {
            if (d.Width, d.Height, d.Format) == (src.Width, src.Height, src.Format) {
                return Ok(have.clone());
            }
        }
        let desc = D3D11_TEXTURE2D_DESC {
            MipLevels: 1,
            ArraySize: 1,
            Usage: if staging {
                D3D11_USAGE_STAGING
            } else {
                D3D11_USAGE_DEFAULT
            },
            BindFlags: if staging {
                0
            } else {
                D3D11_BIND_SHADER_RESOURCE.0 as u32
            },
            CPUAccessFlags: if staging {
                D3D11_CPU_ACCESS_READ.0 as u32
            } else {
                0
            },
            MiscFlags: 0,
            ..src
        };
        let mut made = None;
        // SAFETY: `desc` is a live local and `made` a local out-param checked below.
        let created = unsafe { device.CreateTexture2D(&desc, None, Some(&mut made)) };
        hr(created, "CreateTexture2D")?;
        let made = made.ok_or("null texture")?;
        *slot = Some((made.clone(), desc));
        Ok(made)
    }

    /// One full-frame GPU copy, flushed: about what a convert pass asks of the GPU.
    fn touch(&mut self, tex: &ID3D11Texture2D) -> Result<(), String> {
        let scratch = Self::like(&self.device, &mut self.scratch, tex, false)?;
        // SAFETY: both textures are live, same device, same shape.
        unsafe {
            self.context.CopyResource(&scratch, tex);
            self.context.Flush();
        }
        Ok(())
    }

    /// `(x, y, w, h)` of `tex` as bytes, with the bytes per pixel.
    fn read(
        &mut self,
        tex: &ID3D11Texture2D,
        (x, y, w, h): (u32, u32, u32, u32),
    ) -> Result<(Vec<u8>, usize), String> {
        let staging = Self::like(&self.device, &mut self.staging, tex, true)?;
        let desc = self.staging.as_ref().map(|(_, d)| *d).ok_or("no staging")?;
        if x + w > desc.Width || y + h > desc.Height {
            return Err(format!(
                "read {x},{y},{w}x{h} is outside the {}x{} frame",
                desc.Width, desc.Height
            ));
        }
        let bpp = if desc.Format == DXGI_FORMAT_R16G16B16A16_FLOAT {
            8
        } else {
            4
        };
        let mut out = Vec::with_capacity(w as usize * h as usize * bpp);
        // SAFETY: the copy targets a same-shape staging texture; the mapping is read only inside
        // the bounds checked above (`RowPitch` covers a full row) and unmapped before return.
        unsafe {
            self.context.CopyResource(&staging, tex);
            let mut map = D3D11_MAPPED_SUBRESOURCE::default();
            hr(
                self.context
                    .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut map)),
                "Map(staging)",
            )?;
            for row in y..y + h {
                let start = map
                    .pData
                    .cast::<u8>()
                    .add(row as usize * map.RowPitch as usize + x as usize * bpp);
                out.extend_from_slice(std::slice::from_raw_parts(start, w as usize * bpp));
            }
            self.context.Unmap(&staging, 0);
        }
        Ok((out, bpp))
    }

    /// Does WGC draw the pointer? The same still patch, captured with the cursor on, off, on.
    /// `nudge` moves the pointer one pixel and back first: Windows hides a pointer nobody has
    /// moved, and a hidden pointer answers nothing.
    fn cursor_ab(&mut self, t: &Target, nudge: bool) -> Result<(), String> {
        if nudge {
            for dx in [1, -1] {
                let step = INPUT {
                    r#type: INPUT_MOUSE,
                    Anonymous: INPUT_0 {
                        mi: MOUSEINPUT {
                            dx,
                            dwFlags: MOUSEEVENTF_MOVE,
                            ..Default::default()
                        },
                    },
                };
                // SAFETY: one live `INPUT` of the size passed.
                let sent = unsafe { SendInput(&[step], size_of::<INPUT>() as i32) };
                log("nudge", format!("dx={dx} sent={sent}"));
                std::thread::sleep(Duration::from_millis(60));
            }
        }
        let mut ci = CURSORINFO {
            cbSize: size_of::<CURSORINFO>() as u32,
            ..Default::default()
        };
        // SAFETY: `ci` is a live local with its size set.
        hr(unsafe { GetCursorInfo(&mut ci) }, "GetCursorInfo")?;
        let (mx, my, mw, mh) = t.rect;
        let (px, py) = (ci.ptScreenPos.x - mx, ci.ptScreenPos.y - my);
        let on_monitor = (0..mw).contains(&px) && (0..mh).contains(&py);
        log(
            "cursor",
            format!(
                "flags={:#x} (1=showing 2=suppressed) screen={},{} on_monitor={on_monitor}",
                ci.flags.0, ci.ptScreenPos.x, ci.ptScreenPos.y
            ),
        );
        if !on_monitor {
            log("cursor_ab", "skipped=the pointer is not on this monitor");
            return Ok(());
        }
        const PATCH: i32 = 64;
        let x = (px - PATCH / 2).clamp(0, (mw - PATCH).max(0)) as u32;
        let y = (py - PATCH / 2).clamp(0, (mh - PATCH).max(0)) as u32;
        let patch = (x, y, PATCH.min(mw) as u32, PATCH.min(mh) as u32);
        let mut shots = Vec::new();
        for on in [true, false, true] {
            let t0 = Instant::now();
            hr(
                self.session.SetIsCursorCaptureEnabled(on),
                "SetIsCursorCaptureEnabled",
            )?;
            let frame = self.grab(t.rect)?;
            let (bytes, bpp) = self.read(&texture(&frame)?, patch)?;
            let _ = frame.Close();
            shots.push((bytes, bpp, t0.elapsed()));
        }
        let differing = |a: &[u8], b: &[u8], bpp: usize| {
            a.chunks(bpp)
                .zip(b.chunks(bpp))
                .filter(|(p, q)| p != q)
                .count()
        };
        let bpp = shots[0].1;
        let on_off = differing(&shots[0].0, &shots[1].0, bpp);
        let on_on = differing(&shots[0].0, &shots[2].0, bpp);
        log(
            "cursor_ab",
            format!(
                "patch={},{},{}x{} px_differ_on_vs_off={on_off} px_differ_on_vs_on={on_on} \
                 drawn_by_wgc={} toggle_ms={},{},{}",
                patch.0,
                patch.1,
                patch.2,
                patch.3,
                on_off > 0 && on_on < on_off,
                shots[0].2.as_millis(),
                shots[1].2.as_millis(),
                shots[2].2.as_millis()
            ),
        );
        Ok(())
    }

    /// One settled frame as a BMP. An FP16 frame is also described: its peak and how much of it
    /// is above SDR white says whether the surface really carries scRGB.
    fn shot(&mut self, t: &Target, path: &Path) -> Result<(), String> {
        let frame = self.grab(t.rect)?;
        let saved = texture(&frame).and_then(|tex| self.save(&tex, path));
        let _ = frame.Close();
        saved
    }

    /// `tex` as a BMP at `path`: [`Self::shot`] for a frame the caller already holds.
    fn save(&mut self, tex: &ID3D11Texture2D, path: &Path) -> Result<(), String> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: `tex` is live and `desc` is a live out-param.
        unsafe { tex.GetDesc(&mut desc) };
        let (bytes, bpp) = self.read(tex, (0, 0, desc.Width, desc.Height))?;
        let bgra = if bpp == 8 {
            let (mut peak, mut over1, mut over4, mut negative) = (0f32, 0u64, 0u64, 0u64);
            let mut out = Vec::with_capacity(bytes.len() / 2);
            for px in bytes.chunks_exact(8) {
                let c = |i: usize| half(u16::from_le_bytes([px[i], px[i + 1]]));
                let (r, g, b) = (c(0), c(2), c(4));
                let top = r.max(g).max(b);
                peak = peak.max(top);
                over1 += u64::from(top > 1.0);
                over4 += u64::from(top > 4.0);
                negative += u64::from(r.min(g).min(b) < 0.0);
                // 4.0 is 320 nits in scRGB: enough headroom to see the picture, not to grade it.
                let to8 = |v: f32| ((v / 4.0).clamp(0.0, 1.0).powf(1.0 / 2.2) * 255.0) as u8;
                out.extend_from_slice(&[to8(b), to8(g), to8(r), 255]);
            }
            let n = (bytes.len() / 8).max(1) as f64;
            log(
                "fp16",
                format!(
                    "peak={peak:.3} (1.0=80 nits) over_1_pct={:.2} over_4_pct={:.2} negative_pct={:.2}",
                    over1 as f64 * 100.0 / n,
                    over4 as f64 * 100.0 / n,
                    negative as f64 * 100.0 / n
                ),
            );
            out
        } else {
            bytes
        };
        write_bmp(path, desc.Width, desc.Height, &bgra)
            .map_err(|e| format!("write {path:?}: {e}"))?;
        log(
            "shot",
            format!(
                "path={path:?} size={}x{} bpp={bpp}",
                desc.Width, desc.Height
            ),
        );
        Ok(())
    }
}

/// IEEE 754 binary16 to `f32`.
fn half(h: u16) -> f32 {
    let (sign, exp, frac) = (h >> 15, (h >> 10) & 0x1f, f32::from(h & 0x3ff));
    let v = match exp {
        0 => frac * 2f32.powi(-24),
        31 if frac == 0.0 => f32::INFINITY,
        31 => f32::NAN,
        e => (1.0 + frac / 1024.0) * 2f32.powi(i32::from(e) - 15),
    };
    if sign == 1 {
        -v
    } else {
        v
    }
}

/// Top-down 32-bit BMP from BGRA rows.
fn write_bmp(path: &Path, w: u32, h: u32, bgra: &[u8]) -> std::io::Result<()> {
    let mut file = Vec::with_capacity(54 + bgra.len());
    file.extend_from_slice(b"BM");
    file.extend_from_slice(&(54 + bgra.len() as u32).to_le_bytes());
    file.extend_from_slice(&[0; 4]);
    file.extend_from_slice(&54u32.to_le_bytes());
    file.extend_from_slice(&40u32.to_le_bytes());
    file.extend_from_slice(&(w as i32).to_le_bytes());
    file.extend_from_slice(&(-(h as i32)).to_le_bytes());
    file.extend_from_slice(&1u16.to_le_bytes());
    file.extend_from_slice(&32u16.to_le_bytes());
    file.extend_from_slice(&[0; 24]);
    file.extend_from_slice(bgra);
    std::fs::write(path, file)
}

/// `p` in 0..=100 of `v`, which this sorts.
fn pct(v: &mut [i64], p: usize) -> i64 {
    if v.is_empty() {
        return 0;
    }
    v.sort_unstable();
    v[(v.len() - 1) * p / 100]
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    // SAFETY: forwards the system's own arguments for this window.
    unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
}

/// A 32-pixel square near the monitor's bottom-right corner, repainted a new colour every 2 ms:
/// real pixels change faster than any refresh, so DWM composes every frame it can.
fn animate(rect: Rect, on: &AtomicBool) {
    const SIDE: i32 = 32;
    let (x, y, w, h) = rect;
    let class = w!("wgc-probe-animate");
    // SAFETY: one thread creates the window, paints it, pumps its messages and destroys it.
    // `class` is a static literal and every handle is the checked return of the call before.
    unsafe {
        let Ok(module) = GetModuleHandleW(None) else {
            return;
        };
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wnd_proc),
            hInstance: module.into(),
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassW(&wc);
        let Ok(hwnd) = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            class,
            class,
            WS_POPUP,
            x + w - 2 * SIDE,
            y + h - 4 * SIDE,
            SIDE,
            SIDE,
            None,
            None,
            Some(wc.hInstance),
            None,
        ) else {
            log("animate", "window not created");
            return;
        };
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        let area = RECT {
            left: 0,
            top: 0,
            right: SIDE,
            bottom: SIDE,
        };
        let mut step = 0u32;
        while on.load(Ordering::Acquire) {
            step = step.wrapping_add(0x00_03_07_0b);
            let dc = GetDC(Some(hwnd));
            let brush = CreateSolidBrush(COLORREF(step & 0x00ff_ffff));
            FillRect(dc, &area, brush);
            let _ = DeleteObject(brush.into());
            ReleaseDC(Some(hwnd), dc);
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = DispatchMessageW(&msg);
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        let _ = DestroyWindow(hwnd);
    }
}

/// Only the square, for `secs`: steady damage for another process's measurement.
pub fn animate_only(o: &Opts) -> Result<i32, String> {
    init();
    let t = resolve(&o.monitor)?;
    log("target", &t);
    let on = Arc::new(AtomicBool::new(true));
    let (flag, rect) = (on.clone(), t.rect);
    let painter = std::thread::spawn(move || animate(rect, &flag));
    std::thread::sleep(Duration::from_secs(o.secs));
    on.store(false, Ordering::Release);
    let _ = painter.join();
    Ok(0)
}

/// What DWM says it composes at: the refresh it follows and its own compose rate.
fn dwm_timing() -> String {
    let mut info = DWM_TIMING_INFO {
        cbSize: size_of::<DWM_TIMING_INFO>() as u32,
        ..Default::default()
    };
    // SAFETY: `info` is a live local with its size set; a null window asks for the desktop's.
    if let Err(e) = unsafe { DwmGetCompositionTimingInfo(HWND::default(), &mut info) } {
        return format!("error={:#010x}", e.code().0 as u32);
    }
    // Packed struct: copy each field out before formatting it.
    let (refresh, compose, period) = (info.rateRefresh, info.rateCompose, info.qpcRefreshPeriod);
    // Counters: two readings a known time apart give the refreshes and composed frames between.
    let (refreshes, frames) = (info.cRefresh, info.cFrame);
    let (rn, rd, cn, cd) = (
        refresh.uiNumerator,
        refresh.uiDenominator,
        compose.uiNumerator,
        compose.uiDenominator,
    );
    let mut freq = 0i64;
    // SAFETY: `freq` is a live out-param.
    let _ = unsafe { QueryPerformanceFrequency(&mut freq) };
    // `DwmFlush` returns at DWM's next present: the spacing of 30 of them is the rate it really
    // runs at, whatever the mode says.
    let mut flushes = Vec::new();
    for _ in 0..30 {
        let t0 = Instant::now();
        // SAFETY: no arguments; blocks until the compositor's next present.
        let _ = unsafe { DwmFlush() };
        flushes.push(t0.elapsed().as_micros() as i64);
    }
    format!(
        "refresh={rn}/{rd} compose={cn}/{cd} refresh_period_us={} refresh_count={refreshes} \
         frame_count={frames} flush_us_p50={} flush_us_max={}",
        u128::from(period) * 1_000_000 / (freq.max(1) as u128),
        pct(&mut flushes, 50),
        pct(&mut flushes, 100)
    )
}

/// Wait for the first frame, drawing the canary once `canary_ms` passed with none.
/// `(ms to the frame, whether the canary was drawn)`; `None` is no frame inside the bound.
fn first_frame(s: &mut Session, t: &Target, o: &Opts) -> (Option<u128>, bool) {
    let mut kicked = false;
    loop {
        let frames = s.take(Duration::from_millis(20));
        let waited = s.started.elapsed();
        if !frames.is_empty() {
            for f in frames {
                let _ = f.Close();
            }
            return (Some(waited.as_millis()), kicked);
        }
        if !kicked && o.canary_ms > 0 && waited >= Duration::from_millis(o.canary_ms) {
            kicked = pf_win_display::compose_probe::present(t.rect);
            log(
                "canary",
                format!("drawn={kicked} after_ms={}", waited.as_millis()),
            );
        }
        if waited > FIRST_FRAME {
            return (None, kicked);
        }
    }
}

pub fn capture(o: &Opts) -> Result<i32, String> {
    init();
    let wd = Watchdog::start();
    let t = resolve(&o.monitor)?;
    log("target", &t);
    log("dwm", dwm_timing());
    let mut s = Session::open(&t, o, &wd)?;
    log(
        "open",
        format!(
            "{} size={}x{} format={} buffers={}",
            s.stages(),
            s.size.Width,
            s.size.Height,
            if o.fp16 { "fp16" } else { "bgra" },
            s.buffers
        ),
    );
    let (first, kicked) = first_frame(&mut s, &t, o);
    log(
        "first_frame",
        match first {
            Some(ms) => format!("ms={ms} canary={kicked}"),
            None => format!("none within_ms={} canary={kicked}", FIRST_FRAME.as_millis()),
        },
    );
    if first.is_none() {
        return Ok(5);
    }

    // A still desktop composes nothing, so a rate needs its own damage. Stopped before the
    // cursor and shot legs, which want a still frame.
    let animating = Arc::new(AtomicBool::new(o.animate));
    if o.animate {
        let (on, rect) = (animating.clone(), t.rect);
        std::thread::spawn(move || animate(rect, &on));
    }
    let begun = Instant::now();
    let end = begun + Duration::from_secs(o.secs);
    let (mut ages, mut gaps) = (Vec::<i64>::new(), Vec::<i64>::new());
    let (mut total, mut in_second, mut second) = (0u64, 0u64, 1u64);
    let mut last_time: Option<i64> = None;
    let mut shot_taken = false;
    while Instant::now() < end {
        if s.closed.load(Ordering::Acquire) {
            log(
                "item_closed",
                format!("after_ms={}", begun.elapsed().as_millis()),
            );
            return Ok(4);
        }
        let frames = s.take(Duration::from_millis(100));
        let newest = frames.len().saturating_sub(1);
        for (i, f) in frames.iter().enumerate() {
            let time = f.SystemRelativeTime().map_or(0, |t| t.Duration);
            ages.push((qpc_100ns() - time) / 10);
            if let Some(prev) = last_time.replace(time) {
                gaps.push((time - prev) / 10);
            }
            if let Ok(content) = f.ContentSize() {
                if content != s.size {
                    let t0 = Instant::now();
                    let made = s.pool.Recreate(&s.d3d, s.format, s.buffers, content);
                    log(
                        "content_size",
                        format!(
                            "was={}x{} now={}x{} recreate={:?} recreate_us={}",
                            s.size.Width,
                            s.size.Height,
                            content.Width,
                            content.Height,
                            made.map_err(|e| format!("{:#010x}", e.code().0 as u32)),
                            t0.elapsed().as_micros()
                        ),
                    );
                    s.size = content;
                }
            }
            if o.touch && i == newest {
                s.touch(&texture(f)?)?;
            }
            // The picture as it is this far into the run, whatever the desktop is doing then.
            if let Some(path) = o.shot.as_ref().filter(|_| i == newest && !shot_taken) {
                if o.shot_at > 0 && begun.elapsed() >= Duration::from_secs(o.shot_at) {
                    s.save(&texture(f)?, path)?;
                    shot_taken = true;
                }
            }
            let _ = f.Close();
        }
        total += frames.len() as u64;
        in_second += frames.len() as u64;
        if begun.elapsed() >= Duration::from_secs(second) {
            log(
                "stat",
                format!(
                    "t={second} frames={in_second} arrived={} missed={} input_desktop={}",
                    s.signal.arrived.load(Ordering::Acquire),
                    s.missed,
                    if input_desktop_open() {
                        "open"
                    } else {
                        "denied"
                    }
                ),
            );
            (in_second, second) = (0, second + 1);
        }
    }
    let seconds = begun.elapsed().as_secs_f64().max(0.001);
    log("dwm", dwm_timing());
    // Alone, the shot comes first so it shows the square. With the cursor leg it comes last, so
    // it shows the pointer that leg left switched on.
    if let Some(path) = o.shot.as_ref().filter(|_| !o.cursor_ab && o.shot_at == 0) {
        s.shot(&t, path)?;
    }
    animating.store(false, Ordering::Release);
    if o.animate {
        std::thread::sleep(Duration::from_millis(100));
    }
    if o.cursor_ab {
        s.cursor_ab(&t, o.nudge)?;
        if let Some(path) = o.shot.as_ref().filter(|_| o.shot_at == 0) {
            s.shot(&t, path)?;
        }
    }
    log(
        "summary",
        format!(
            "frames={total} fps={:.2} missed={} age_us_p50={} age_us_p99={} age_us_max={} \
             gap_us_p50={} gap_us_p99={} gap_us_max={}",
            total as f64 / seconds,
            s.missed,
            pct(&mut ages, 50),
            pct(&mut ages, 99),
            pct(&mut ages, 100),
            pct(&mut gaps, 50),
            pct(&mut gaps, 99),
            pct(&mut gaps, 100),
        ),
    );
    Ok(0)
}

/// Open, wait for a frame, close — `count` times. The per-stage spread and the number of opens
/// that needed the canary are the result; a hang ends the process through the watchdog.
pub fn opens(o: &Opts) -> Result<i32, String> {
    init();
    let wd = Watchdog::start();
    let t = resolve(&o.monitor)?;
    log("target", &t);
    let mut per_stage: BTreeMap<&'static str, Vec<i64>> = BTreeMap::new();
    let (mut firsts, mut kicks, mut none, mut failed) = (Vec::<i64>::new(), 0u32, 0u32, 0u32);
    for i in 0..o.count {
        let t0 = Instant::now();
        let mut s = match Session::open(&t, o, &wd) {
            Ok(s) => s,
            Err(e) => {
                failed += 1;
                log("open_failed", format!("iter={i} {e}"));
                continue;
            }
        };
        for (name, d) in &s.stages {
            per_stage
                .entry(name)
                .or_default()
                .push(d.as_micros() as i64);
        }
        let (first, kicked) = first_frame(&mut s, &t, o);
        kicks += u32::from(kicked);
        match first {
            Some(ms) => firsts.push(ms as i64),
            None => none += 1,
        }
        let stages = s.stages();
        let (_, closing) = wd.stage("close", || drop(s));
        if t0.elapsed() > Duration::from_secs(1) || first.is_none() {
            log(
                "slow",
                format!(
                    "iter={i} total_ms={} first_frame_ms={first:?} canary={kicked} close_us={} {stages}",
                    t0.elapsed().as_millis(),
                    closing.as_micros()
                ),
            );
        }
    }
    for (name, v) in &mut per_stage {
        log(
            "stage",
            format!(
                "name={name} n={} p50_us={} p99_us={} max_us={}",
                v.len(),
                pct(v, 50),
                pct(v, 99),
                pct(v, 100)
            ),
        );
    }
    log(
        "summary",
        format!(
            "opens={} failed={failed} no_first_frame={none} needed_canary={kicks} \
             first_frame_ms_p50={} first_frame_ms_max={}",
            o.count,
            pct(&mut firsts, 50),
            pct(&mut firsts, 100)
        ),
    );
    Ok(if failed + none == 0 { 0 } else { 6 })
}
