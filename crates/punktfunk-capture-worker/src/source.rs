//! The Windows Graphics Capture side: one monitor, opened on the adapter that drives it, its
//! frames handed to whichever pool the live encode session attached.
//!
//! A capture delivers only when the desktop changes, and its first frame arrives at
//! `StartCapture`, before any encoder exists. So the newest frame is always kept open
//! ([`Feed::last`]): a session that opens over a still desktop gets it as its first picture.
//! Evidence: `design/windows-wgc-capture.md` §4.4 and the measurements in §7.1.

use std::mem::offset_of;
use std::sync::{Arc, Mutex};

use pf_driver_proto::encode::au::{self, AuHeader};
use pf_driver_proto::worker as proto;
use pf_encode_session::lock;
use pf_encode_session::open::{qpc_frequency, AdapterId};
use pf_encode_session::section::EncodeSession;
use windows::core::{h, IInspectable, Interface, HSTRING};
use windows::Foundation::Metadata::ApiInformation;
use windows::Foundation::{TimeSpan, TypedEventHandler};
use windows::Graphics::Capture::{
    Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureAccess,
    GraphicsCaptureAccessKind, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread, ID3D11Texture2D,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R16G16B16A16_FLOAT,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter1, IDXGIDevice, IDXGIFactory1,
};
use windows::Win32::Graphics::Gdi::{
    EnumDisplaySettingsW, DEVMODEW, ENUM_CURRENT_SETTINGS, HMONITOR,
};
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;

use crate::pool::{Offer, Pool};

const SESSION_CLASS: &HSTRING = h!("Windows.Graphics.Capture.GraphicsCaptureSession");

/// Capture buffers. One is the frame kept open, one is the frame arriving.
const BUFFERS: i32 = 2;

/// Why a source did not open: the reply's status, the failing call's HRESULT, a stage tag.
pub type Refusal = (u32, i32, &'static str);

fn failed(stage: &'static str) -> impl Fn(windows::core::Error) -> Refusal {
    move |e| {
        tracing::warn!(stage, error = %e, "capture source did not open");
        (proto::SOURCE_OPEN_FAILED, e.code().0, stage)
    }
}

/// The output named `gdi`, with the adapter that enumerates it.
fn find_output(gdi: &str) -> Result<(IDXGIAdapter1, HMONITOR), Refusal> {
    // SAFETY: plain DXGI enumeration; each interface is the checked return of the call before.
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().map_err(failed("factory"))?;
        let mut a = 0;
        while let Ok(adapter) = factory.EnumAdapters1(a) {
            a += 1;
            let mut o = 0;
            while let Ok(output) = adapter.EnumOutputs(o) {
                o += 1;
                let Ok(desc) = output.GetDesc() else {
                    continue;
                };
                if proto::gdi_name_text(&desc.DeviceName).eq_ignore_ascii_case(gdi) {
                    return Ok((adapter, desc.Monitor));
                }
            }
        }
    }
    Err((proto::SOURCE_NOT_FOUND, 0, "output"))
}

/// The output's refresh in whole Hz; `0` when the mode does not say.
fn refresh_hz(gdi: &[u16; 32]) -> u32 {
    let mut mode = DEVMODEW {
        dmSize: size_of::<DEVMODEW>() as u16,
        ..Default::default()
    };
    // SAFETY: `gdi` is NUL-terminated inside its 32 units (the request was checked), and
    // `mode` is a sized local out-param.
    let ok = unsafe {
        EnumDisplaySettingsW(
            windows::core::PCWSTR(gdi.as_ptr()),
            ENUM_CURRENT_SETTINGS,
            &mut mode,
        )
    };
    if ok.as_bool() {
        mode.dmDisplayFrequency
    } else {
        0
    }
}

fn make_device(
    adapter: &IDXGIAdapter1,
) -> windows::core::Result<(ID3D11Device, ID3D11DeviceContext)> {
    let (mut device, mut context) = (None, None);
    // SAFETY: `adapter` is live for the call; the out-params are local `Option`s checked below.
    unsafe {
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
        )?;
    }
    let (Some(device), Some(context)) = (device, context) else {
        return Err(windows::core::Error::empty());
    };
    // The arrival handler and the encode thread share the immediate context.
    if let Ok(mt) = context.cast::<ID3D11Multithread>() {
        // SAFETY: a plain setter on the live context.
        let _ = unsafe { mt.SetMultithreadProtected(true) };
    }
    Ok((device, context))
}

fn texture(frame: &Direct3D11CaptureFrame) -> windows::core::Result<ID3D11Texture2D> {
    let access: IDirect3DDxgiInterfaceAccess = frame.Surface()?.cast()?;
    // SAFETY: `access` is live; the call returns an owned, checked interface.
    unsafe { access.GetInterface() }
}

/// A frame the capture still counts as out, with its surface.
struct Kept {
    frame: Direct3D11CaptureFrame,
    tex: ID3D11Texture2D,
}

impl Drop for Kept {
    /// Closing is what gives the buffer back to the capture.
    fn drop(&mut self) {
        let _ = self.frame.Close();
    }
}

/// What the arrival handler writes to, and what the control thread swaps under it.
#[derive(Default)]
struct Feed {
    /// The live session's pool and the session whose header its counters go to.
    sink: Option<(Arc<Pool>, Arc<EncodeSession>)>,
    /// The newest frame, kept open for the next pool to open on.
    last: Option<Kept>,
    /// The size the capture's buffers are, which a frame's content size is checked against.
    size: (i32, i32),
}

/// The capture's WinRT view of the D3D11 device.
struct CaptureDevice(IDirect3DDevice);

// SAFETY: it wraps a D3D11 device, which is free-threaded, and a free-threaded frame pool is
// created on exactly that promise: the pool itself calls into it from its own threads.
unsafe impl Send for CaptureDevice {}
// SAFETY: as above.
unsafe impl Sync for CaptureDevice {}

/// What the handler needs besides the feed: the pieces a resize rebuilds the buffers from.
struct Shared {
    feed: Mutex<Feed>,
    d3d: CaptureDevice,
    format: DirectXPixelFormat,
    /// QPC ticks per second, for the frames' 100 ns stamps.
    qpc_hz: u64,
    /// Called with the new size after the capture's buffers were rebuilt for it.
    on_resize: Box<dyn Fn(u32, u32) + Send + Sync>,
}

impl Shared {
    fn offer(feed: &Feed, tex: &ID3D11Texture2D, qpc: u64) {
        let Some((pool, session)) = &feed.sink else {
            return;
        };
        let section = &session.section;
        match pool.offer(tex, qpc) {
            Offer::Taken(seq, recycled) => {
                section.store_u64(offset_of!(AuHeader, source_seq), seq);
                if let Some(n) = recycled {
                    section.store_u64(offset_of!(AuHeader, dropped_total), n);
                }
            }
            Offer::Dropped(n) => section.store_u64(offset_of!(AuHeader, dropped_total), n),
            // The session was opened for another size or format: say so where the host looks.
            Offer::Refused => {
                if !session
                    .stale
                    .swap(true, std::sync::atomic::Ordering::AcqRel)
                {
                    section.store_u32(offset_of!(AuHeader, encoder_state), au::ENCODER_WEDGED);
                }
            }
        }
    }

    /// The pool's arrival callback: keep the newest frame, hand it to the live pool.
    fn arrived(&self, pool: &Direct3D11CaptureFramePool) {
        let mut newest = None;
        while let Ok(frame) = pool.TryGetNextFrame() {
            if let Some(older) = newest.replace(frame) {
                let _ = older.Close();
            }
        }
        let Some(frame) = newest else {
            return;
        };
        let mut feed = lock(&self.feed);
        let size = frame.ContentSize().unwrap_or(SizeInt32 {
            Width: feed.size.0,
            Height: feed.size.1,
        });
        if (size.Width, size.Height) != feed.size && size.Width > 0 && size.Height > 0 {
            // The source changed mode. Its frames no longer fit the buffers or the session.
            let _ = frame.Close();
            feed.last = None;
            feed.sink = None;
            if pool
                .Recreate(&self.d3d.0, self.format, BUFFERS, size)
                .is_ok()
            {
                feed.size = (size.Width, size.Height);
                drop(feed);
                (self.on_resize)(size.Width as u32, size.Height as u32);
            }
            return;
        }
        let Ok(tex) = texture(&frame) else {
            let _ = frame.Close();
            return;
        };
        // 100 ns units since boot, the clock QPC counts on.
        let stamp = frame.SystemRelativeTime().map_or(0, |t| t.Duration.max(0));
        let qpc = (stamp as u128 * u128::from(self.qpc_hz) / 10_000_000) as u64;
        Self::offer(&feed, &tex, qpc);
        feed.last = Some(Kept { frame, tex });
    }
}

/// One open capture. Dropping it stops the capture and gives its buffers back.
pub struct Source {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
    pub adapter: AdapterId,
    /// What the frames are: the surface format a session's input kind must read.
    pub format: DXGI_FORMAT,
    /// The output's refresh in Hz, for the loop's pacing; `0` when unknown.
    pub refresh_hz: u32,
    shared: Arc<Shared>,
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    _item: GraphicsCaptureItem,
}

impl Drop for Source {
    fn drop(&mut self) {
        let _ = self.session.Close();
        let _ = self.pool.Close();
        let mut feed = lock(&self.shared.feed);
        feed.sink = None;
        feed.last = None;
    }
}

impl Source {
    /// Open the capture `req` names. Every stage is a call that has hung on some box; the host
    /// bounds the whole open and ends this process when it runs over.
    pub fn open(
        req: &proto::OpenSource,
        on_resize: Box<dyn Fn(u32, u32) + Send + Sync>,
    ) -> Result<Self, Refusal> {
        let known = proto::OPEN_FP16 | proto::OPEN_CURSOR;
        let gdi = proto::gdi_name_text(&req.gdi_name);
        let terminated = req.gdi_name.contains(&0);
        if req.flags & !known != 0 || gdi.is_empty() || !terminated || req.frame_interval_100ns == 0
        {
            return Err((proto::SOURCE_BAD_REQUEST, 0, "request"));
        }
        let fp16 = req.flags & proto::OPEN_FP16 != 0;
        let (format, dxgi_format) = if fp16 {
            (
                DirectXPixelFormat::R16G16B16A16Float,
                DXGI_FORMAT_R16G16B16A16_FLOAT,
            )
        } else {
            (
                DirectXPixelFormat::B8G8R8A8UIntNormalized,
                DXGI_FORMAT_B8G8R8A8_UNORM,
            )
        };

        let (adapter, monitor) = find_output(&gdi)?;
        let (device, context) = make_device(&adapter).map_err(failed("device"))?;
        let adapter = AdapterId::of(&device).ok_or((proto::SOURCE_OPEN_FAILED, 0, "adapter"))?;
        let dxgi: IDXGIDevice = device.cast().map_err(failed("dxgi"))?;
        // SAFETY: `dxgi` is a live interface; the call returns an owned, checked object.
        let d3d: IDirect3DDevice = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi) }
            .and_then(|inspectable| inspectable.cast())
            .map_err(failed("wrap"))?;
        let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()
            .map_err(failed("factory"))?;
        // SAFETY: `monitor` came from this process's own DXGI enumeration a moment ago.
        let item: GraphicsCaptureItem =
            unsafe { interop.CreateForMonitor(monitor) }.map_err(failed("item"))?;
        let size = item.Size().map_err(failed("size"))?;
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(&d3d, format, BUFFERS, size)
            .map_err(failed("pool"))?;

        let shared = Arc::new(Shared {
            feed: Mutex::new(Feed {
                size: (size.Width, size.Height),
                ..Feed::default()
            }),
            d3d: CaptureDevice(d3d),
            format,
            qpc_hz: qpc_frequency(),
            on_resize,
        });
        let handler = shared.clone();
        pool.FrameArrived(
            &TypedEventHandler::<Direct3D11CaptureFramePool, IInspectable>::new(move |pool, _| {
                if let Some(pool) = pool.as_ref() {
                    handler.arrived(pool);
                }
                Ok(())
            }),
        )
        .map_err(failed("arrived"))?;

        let session = pool
            .CreateCaptureSession(&item)
            .map_err(failed("session"))?;
        let has = |p: &str| {
            ApiInformation::IsPropertyPresent(SESSION_CLASS, &HSTRING::from(p)).unwrap_or(false)
        };
        // The border may only be cleared after the access request; a refusal leaves it drawn.
        let access =
            GraphicsCaptureAccess::RequestAccessAsync(GraphicsCaptureAccessKind::Borderless)
                .and_then(|op| op.join());
        let borderless = has("IsBorderRequired") && session.SetIsBorderRequired(false).is_ok();
        session
            .SetIsCursorCaptureEnabled(req.flags & proto::OPEN_CURSOR != 0)
            .map_err(failed("cursor"))?;
        // Half the frame period: an interval equal to it misses ticks. Without the property the
        // capture runs at the output's refresh and the loop sheds the surplus.
        let paced = has("MinUpdateInterval")
            && session
                .SetMinUpdateInterval(TimeSpan {
                    Duration: i64::from(req.frame_interval_100ns / 2).max(1),
                })
                .is_ok();
        session.StartCapture().map_err(failed("start"))?;
        let refresh_hz = refresh_hz(&req.gdi_name);
        tracing::info!(
            gdi = %gdi,
            width = size.Width,
            height = size.Height,
            fp16,
            refresh_hz,
            access = ?access.map(|s| s.0),
            borderless,
            paced,
            "capture source open"
        );
        Ok(Self {
            device,
            context,
            adapter,
            format: dxgi_format,
            refresh_hz,
            shared,
            pool,
            session,
            _item: item,
        })
    }

    /// The size the capture's frames are now.
    pub fn size(&self) -> (u32, u32) {
        let (w, h) = lock(&self.shared.feed).size;
        (w as u32, h as u32)
    }

    /// Whether the capture draws the pointer into its frames, from the next one on.
    pub fn set_cursor(&self, in_picture: bool) -> bool {
        self.session.SetIsCursorCaptureEnabled(in_picture).is_ok()
    }

    /// Feed `pool` from now on, starting with the frame already held: a still desktop sends
    /// no other. Its stamp is `0`, so the loop stamps it with now.
    pub fn attach(&self, pool: Arc<Pool>, session: Arc<EncodeSession>) {
        let mut feed = lock(&self.shared.feed);
        feed.sink = Some((pool, session));
        if let Some(kept) = &feed.last {
            Shared::offer(&feed, &kept.tex, 0);
        }
    }

    /// Stop feeding `pool`, if it is still the one being fed.
    pub fn detach(&self, pool: &Arc<Pool>) {
        let mut feed = lock(&self.shared.feed);
        if feed
            .sink
            .as_ref()
            .is_some_and(|(p, _)| Arc::ptr_eq(p, pool))
        {
            feed.sink = None;
        }
    }

    /// Wake the live session's loop for a control op.
    pub fn wake(&self) {
        if let Some((pool, _)) = &lock(&self.shared.feed).sink {
            pool.wake();
        }
    }
}
