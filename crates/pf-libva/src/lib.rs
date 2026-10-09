//! The libva runtime: `libva.so.2` and `libva-drm.so.2` opened at run time, a
//! `VADisplay` over a DRM render node, and the entrypoint probe.
//!
//! Its own crate for two reasons. [`pf_vaapi`] declares `#![forbid(unsafe_code)]` —
//! that is the contract making its parameter-buffer half testable on any OS — and
//! this is unsafe FFI, so it cannot live there. And both the client's decode rung and
//! the host's VAAPI encoder need the same dlopen, the same node-selection rule and the
//! same entrypoint question; two copies would answer them differently, which is how a
//! decoder ends up on a different GPU than the presenter.
//!
//! A missing libva is a clean `Err`, never a link error: the decode and encode ladders
//! both fall through it.

use std::os::fd::AsRawFd as _;
/// An H.264 encode session driven with `pf-vaapi`'s parameter buffers.
pub mod encode;
/// The VideoProc context that turns capture pictures into encoder input.
pub mod vpp;

use std::os::fd::OwnedFd;
use std::os::raw::c_char;
use std::os::raw::c_int;
use std::os::raw::c_uint;
use std::os::raw::c_void;

use anyhow::anyhow;
use anyhow::bail;
use anyhow::Context as _;
use anyhow::Result;
use pf_vaapi::drm::ExportedPlane;
use pf_vaapi::drm::VaDrmPrimeObject;
use pf_vaapi::drm::VaDrmPrimeSurfaceDescriptor;
use pf_vaapi::drm::MAX_OBJECTS;
use pf_vaapi::drm::MAX_PLANES_PER_LAYER;
use pf_vaapi::va::VaImage;
use pf_vaapi::vpp::VA_GENERIC_VALUE_TYPE_POINTER;
use pf_vaapi::vpp::VA_SURFACE_ATTRIB_EXTERNAL_BUFFER_DESCRIPTOR;
use pf_vaapi::vpp::VA_SURFACE_ATTRIB_MEMORY_TYPE;
use pf_vaapi::vpp::VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2;

pub type VaDisplay = *mut c_void;
pub type VaStatus = c_int;
pub type VaSurfaceId = c_uint;
pub type VaConfigId = c_uint;
pub type VaContextId = c_uint;
pub type VaBufferId = c_uint;

pub const VA_STATUS_SUCCESS: VaStatus = 0;
/// Also the "no surface" sentinel in a slot table. 0 is a plausible `VASurfaceID`.
pub const VA_INVALID_ID: c_uint = 0xffff_ffff;
/// The only picture structure this rung's envelope contains.
pub const VA_PROGRESSIVE: c_uint = 0x0001;

/// `VAGenericValue`: 16 bytes, value at offset 8, align 8 (`pf-vaapi/layout-probe.c`).
///
/// The C `value` is a union that includes a pointer, so it is eight-byte aligned —
/// four bytes of padding after `kind`, 16 bytes total not 12. The union is kept as
/// the one word it is, so every byte crossing the FFI was written here; the
/// integer arm is its low half on the little-endian targets this runs on.
#[repr(C, align(8))]
#[derive(Clone, Copy)]
pub struct VaGenericValue {
    pub kind: c_int,
    pub _pad: u32,
    bits: u64,
}

impl VaGenericValue {
    /// The `int` arm: a fourcc, a memory type.
    pub fn integer(i: i32) -> Self {
        Self {
            kind: VA_GENERIC_VALUE_TYPE_INTEGER,
            _pad: 0,
            bits: u64::from(i as u32),
        }
    }

    /// The pointer arm: an external buffer descriptor the call reads, not keeps.
    pub fn pointer(p: *mut c_void) -> Self {
        Self {
            kind: VA_GENERIC_VALUE_TYPE_POINTER,
            _pad: 0,
            bits: p as usize as u64,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VaSurfaceAttrib {
    pub kind: c_int,
    pub flags: c_uint,
    pub value: VaGenericValue,
}

/// `VAConfigAttrib`: a type/value pair, the shape `vaCreateConfig` and
/// `vaGetConfigAttributes` take. `kind` is a `pf_vaapi` `VA_CONFIG_ATTRIB_*`.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct VaConfigAttrib {
    pub kind: u32,
    pub value: u32,
}

/// Measured by `pf-vaapi/layout-probe.c`.
pub const VA_SURFACE_ATTRIB_PIXEL_FORMAT: c_int = 1;
pub const VA_GENERIC_VALUE_TYPE_INTEGER: c_int = 1;
pub const VA_SURFACE_ATTRIB_SETTABLE: c_uint = 0x0002;

// Layouts passed by value, measured (`pf-vaapi/layout-probe.c`).
const _: () = {
    assert!(size_of::<VaGenericValue>() == 16);
    assert!(std::mem::offset_of!(VaGenericValue, bits) == 8);
    assert!(size_of::<VaSurfaceAttrib>() == 24);
    assert!(std::mem::offset_of!(VaSurfaceAttrib, flags) == 4);
    assert!(std::mem::offset_of!(VaSurfaceAttrib, value) == 8);
    assert!(size_of::<VaConfigAttrib>() == 8);
    assert!(std::mem::offset_of!(VaConfigAttrib, value) == 4);
};

/// libva entry points from `libva.so.2` / `libva-drm.so.2`. Absent library is a
/// clean refusal, not a link error.
pub struct Libva {
    pub _va: libloading::Library,
    pub _drm: libloading::Library,
    pub get_display_drm: unsafe extern "C" fn(c_int) -> VaDisplay,
    pub initialize: unsafe extern "C" fn(VaDisplay, *mut c_int, *mut c_int) -> VaStatus,
    pub terminate: unsafe extern "C" fn(VaDisplay) -> VaStatus,
    pub error_str: unsafe extern "C" fn(VaStatus) -> *const c_char,
    pub query_config_entrypoints:
        unsafe extern "C" fn(VaDisplay, c_int, *mut c_int, *mut c_int) -> VaStatus,
    pub max_entrypoints: unsafe extern "C" fn(VaDisplay) -> c_int,
    pub create_config: unsafe extern "C" fn(
        VaDisplay,
        c_int,
        c_int,
        *mut c_void,
        c_int,
        *mut VaConfigId,
    ) -> VaStatus,
    pub destroy_config: unsafe extern "C" fn(VaDisplay, VaConfigId) -> VaStatus,
    pub create_surfaces: unsafe extern "C" fn(
        VaDisplay,
        c_uint,
        c_uint,
        c_uint,
        *mut VaSurfaceId,
        c_uint,
        *mut VaSurfaceAttrib,
        c_uint,
    ) -> VaStatus,
    pub destroy_surfaces: unsafe extern "C" fn(VaDisplay, *mut VaSurfaceId, c_int) -> VaStatus,
    pub create_context: unsafe extern "C" fn(
        VaDisplay,
        VaConfigId,
        c_int,
        c_int,
        c_int,
        *mut VaSurfaceId,
        c_int,
        *mut VaContextId,
    ) -> VaStatus,
    pub destroy_context: unsafe extern "C" fn(VaDisplay, VaContextId) -> VaStatus,
    pub create_buffer: unsafe extern "C" fn(
        VaDisplay,
        VaContextId,
        c_uint,
        c_uint,
        c_uint,
        *mut c_void,
        *mut VaBufferId,
    ) -> VaStatus,
    pub destroy_buffer: unsafe extern "C" fn(VaDisplay, VaBufferId) -> VaStatus,
    pub begin_picture: unsafe extern "C" fn(VaDisplay, VaContextId, VaSurfaceId) -> VaStatus,
    pub render_picture:
        unsafe extern "C" fn(VaDisplay, VaContextId, *mut VaBufferId, c_int) -> VaStatus,
    pub end_picture: unsafe extern "C" fn(VaDisplay, VaContextId) -> VaStatus,
    pub sync_surface: unsafe extern "C" fn(VaDisplay, VaSurfaceId) -> VaStatus,
    /// Buffer-specific encode completion (libva >= 1.9); `None` keeps the surface fallback.
    pub sync_buffer: Option<unsafe extern "C" fn(VaDisplay, VaBufferId, u64) -> VaStatus>,
    /// Non-blocking half of `sync_surface`: the surface's `VASurfaceStatus` out.
    pub query_surface_status: unsafe extern "C" fn(VaDisplay, VaSurfaceId, *mut c_int) -> VaStatus,
    pub export_surface_handle:
        unsafe extern "C" fn(VaDisplay, VaSurfaceId, c_uint, c_uint, *mut c_void) -> VaStatus,
    /// Encode reads its bitstream back through a mapped coded buffer; decode has no
    /// use for either, so both arrived with the encoder.
    pub map_buffer: unsafe extern "C" fn(VaDisplay, VaBufferId, *mut *mut c_void) -> VaStatus,
    pub unmap_buffer: unsafe extern "C" fn(VaDisplay, VaBufferId) -> VaStatus,
    /// Writing pixels into a surface without a dmabuf: derive its image, map, fill.
    /// The encoder's real ingest is a dmabuf import; this is how tests make frames.
    pub derive_image: unsafe extern "C" fn(VaDisplay, VaSurfaceId, *mut c_void) -> VaStatus,
    pub destroy_image: unsafe extern "C" fn(VaDisplay, u32) -> VaStatus,
    pub query_config_attributes: unsafe extern "C" fn(
        VaDisplay,
        VaConfigId,
        *mut c_int,
        *mut c_int,
        *mut c_void,
        *mut c_int,
    ) -> VaStatus,
    pub get_config_attributes:
        unsafe extern "C" fn(VaDisplay, c_int, c_int, *mut c_void, c_int) -> VaStatus,
}

impl Libva {
    pub fn load() -> Result<Libva> {
        // SAFETY: `Library::new` runs the trusted system libva's initialisers, and each
        // `lib.get` resolves a documented libva symbol to the matching `unsafe extern "C"`
        // signature transcribed from `va.h` / `va_drm.h` (by-value integers and pointers
        // throughout, no callbacks). Both `Library` handles are stored in the returned
        // struct, so every resolved pointer outlives its uses.
        unsafe {
            let va = libloading::Library::new("libva.so.2")
                .context("libva.so.2 (no VAAPI runtime on this system)")?;
            let drm = libloading::Library::new("libva-drm.so.2")
                .context("libva-drm.so.2 (no VAAPI DRM backend on this system)")?;
            // Resolved at the field's own type — no `transmute`. Bound with `let`
            // so each `Library` borrow ends before the handle moves into the struct.
            macro_rules! get {
                ($lib:expr, $name:literal) => {
                    *$lib
                        .get(concat!($name, "\0").as_bytes())
                        .map_err(|e| anyhow!(concat!("dlsym ", $name, ": {}"), e))?
                };
            }
            let get_display_drm = get!(drm, "vaGetDisplayDRM");
            let initialize = get!(va, "vaInitialize");
            let terminate = get!(va, "vaTerminate");
            let error_str = get!(va, "vaErrorStr");
            let query_config_entrypoints = get!(va, "vaQueryConfigEntrypoints");
            let max_entrypoints = get!(va, "vaMaxNumEntrypoints");
            let create_config = get!(va, "vaCreateConfig");
            let destroy_config = get!(va, "vaDestroyConfig");
            let create_surfaces = get!(va, "vaCreateSurfaces");
            let destroy_surfaces = get!(va, "vaDestroySurfaces");
            let create_context = get!(va, "vaCreateContext");
            let destroy_context = get!(va, "vaDestroyContext");
            let create_buffer = get!(va, "vaCreateBuffer");
            let destroy_buffer = get!(va, "vaDestroyBuffer");
            let begin_picture = get!(va, "vaBeginPicture");
            let render_picture = get!(va, "vaRenderPicture");
            let end_picture = get!(va, "vaEndPicture");
            let sync_surface = get!(va, "vaSyncSurface");
            let sync_buffer: Option<unsafe extern "C" fn(VaDisplay, VaBufferId, u64) -> VaStatus> =
                va.get(b"vaSyncBuffer\0").ok().map(|symbol| *symbol);
            let query_surface_status = get!(va, "vaQuerySurfaceStatus");
            let export_surface_handle = get!(va, "vaExportSurfaceHandle");
            let map_buffer = get!(va, "vaMapBuffer");
            let unmap_buffer = get!(va, "vaUnmapBuffer");
            let derive_image = get!(va, "vaDeriveImage");
            let destroy_image = get!(va, "vaDestroyImage");
            let query_config_attributes = get!(va, "vaQueryConfigAttributes");
            let get_config_attributes = get!(va, "vaGetConfigAttributes");
            Ok(Libva {
                get_display_drm,
                initialize,
                terminate,
                error_str,
                query_config_entrypoints,
                max_entrypoints,
                create_config,
                destroy_config,
                create_surfaces,
                destroy_surfaces,
                create_context,
                destroy_context,
                create_buffer,
                destroy_buffer,
                begin_picture,
                render_picture,
                end_picture,
                sync_surface,
                sync_buffer,
                query_surface_status,
                export_surface_handle,
                map_buffer,
                unmap_buffer,
                derive_image,
                destroy_image,
                query_config_attributes,
                get_config_attributes,
                _va: va,
                _drm: drm,
            })
        }
    }

    pub fn err(&self, what: &str, status: VaStatus) -> anyhow::Error {
        // SAFETY: `vaErrorStr` is documented total — it returns a pointer into libva's
        // static string table for any input, valid while the library is loaded, which
        // `&self` proves.
        let text = unsafe {
            let p = (self.error_str)(status);
            if p.is_null() {
                String::new()
            } else {
                std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned()
            }
        };
        if text.is_empty() {
            anyhow!("{what} failed ({status})")
        } else {
            anyhow!("{what} failed: {text} ({status})")
        }
    }

    pub fn check(&self, what: &str, status: VaStatus) -> Result<()> {
        if status == VA_STATUS_SUCCESS {
            Ok(())
        } else {
            Err(self.err(what, status))
        }
    }
}

/// Initialised `VADisplay` over a DRM render node.
pub struct Display {
    pub va: Libva,
    pub display: VaDisplay,
    /// libva does not dup the fd; the display is valid only while this stays open,
    /// and it is dropped after `vaTerminate`.
    pub node: Option<OwnedFd>,
    pub path: String,
    pub version: (c_int, c_int),
}

// SAFETY: the display is created and used from ONE thread (the pump), and `Send` only
// permits MOVING that ownership. libva is not safe for concurrent calls on one
// display, which is why `Sync` is deliberately absent: every path into it goes through
// `&mut NativeVaapiDecoder`, and that is the serialisation.
unsafe impl Send for Display {}

/// PCI vendor of a render node in Vulkan `vendorID` units. A platform or
/// virtual GPU with no PCI vendor returns `None` and stays in the fallback set.
fn node_vendor_id(node: &std::path::Path) -> Option<u32> {
    let name = node.file_name()?.to_str()?;
    let text = std::fs::read_to_string(format!("/sys/class/drm/{name}/device/vendor")).ok()?;
    u32::from_str_radix(text.trim().trim_start_matches("0x"), 16).ok()
}

impl Display {
    /// Open the first VAAPI render node in name order. The environment pin wins.
    pub fn open(va: Libva) -> Result<Display> {
        Self::open_for_vendor(va, None)
    }

    /// `PUNKTFUNK_VAAPI_DEVICE` pins a node; otherwise nodes matching the
    /// presenter's PCI vendor lead each name-ordered probe. Unknown nodes follow.
    pub fn open_for_vendor(va: Libva, preferred_vendor: Option<u32>) -> Result<Display> {
        if let Some(pin) = std::env::var_os("PUNKTFUNK_VAAPI_DEVICE") {
            let path = pin.to_string_lossy().into_owned();
            let (display, node, version) = Display::probe(&va, &path)
                .with_context(|| format!("PUNKTFUNK_VAAPI_DEVICE={path}"))?;
            return Ok(Display {
                va,
                display,
                node: Some(node),
                path,
                version,
            });
        }
        let mut nodes: Vec<std::path::PathBuf> = std::fs::read_dir("/dev/dri")
            .context("/dev/dri (no DRM devices on this machine)")?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("renderD"))
            })
            .collect();
        nodes.sort();
        // Stable partition: name order still decides inside each vendor group.
        if let Some(want) = preferred_vendor {
            nodes.sort_by_key(|node| node_vendor_id(node) != Some(want));
        }
        let mut tried: Vec<String> = Vec::new();
        for node in &nodes {
            let path = node.to_string_lossy().into_owned();
            match Display::probe(&va, &path) {
                Ok((display, node, version)) => {
                    return Ok(Display {
                        va,
                        display,
                        node: Some(node),
                        path,
                        version,
                    })
                }
                Err(e) => {
                    tracing::debug!(node = %path, reason = %format!("{e:#}"), "not a VAAPI device");
                    tried.push(path);
                }
            }
        }
        bail!(
            "no render node initialised a VAAPI display ({})",
            if tried.is_empty() {
                "/dev/dri has no renderD* nodes".to_string()
            } else {
                format!("tried {}", tried.join(", "))
            }
        )
    }

    /// The node the caller chose — the host's `pf_gpu::linux_render_node()` — and
    /// no other.
    pub fn open_path(va: Libva, path: &str) -> Result<Display> {
        let (display, node, version) =
            Display::probe(&va, path).with_context(|| path.to_string())?;
        Ok(Display {
            va,
            display,
            node: Some(node),
            path: path.to_string(),
            version,
        })
    }

    /// One node, borrowing the already-loaded library.
    pub fn probe(va: &Libva, path: &str) -> Result<(VaDisplay, OwnedFd, (c_int, c_int))> {
        let node = OwnedFd::from(
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(path)
                .with_context(|| format!("open {path}"))?,
        );
        // SAFETY: `vaGetDisplayDRM` takes the render node's fd by value and returns an
        // opaque display or null; `vaInitialize` writes the two version ints through
        // the out-pointers, which are locals live across the call. The fd stays open in
        // `node` for as long as the display exists — libva does not dup it.
        unsafe {
            let display = (va.get_display_drm)(node.as_raw_fd());
            if display.is_null() {
                bail!("vaGetDisplayDRM({path}) returned no display");
            }
            let (mut major, mut minor) = (0, 0);
            let status = (va.initialize)(display, &mut major, &mut minor);
            if status != VA_STATUS_SUCCESS {
                let e = va.err("vaInitialize", status);
                // Unusable but still allocated; terminate so the driver state goes too.
                (va.terminate)(display);
                // Callers already name the node; putting it in the error printed it twice.
                return Err(e);
            }
            Ok((display, node, (major, minor)))
        }
    }
}

impl Display {
    /// Asked before `vaCreateConfig` so an unsupported profile is a named refusal,
    /// not a driver status code.
    pub fn require_entrypoint(&self, profile: c_int) -> Result<()> {
        if !self
            .entrypoints(profile)?
            .contains(&pf_vaapi::VA_ENTRYPOINT_VLD)
        {
            bail!("this device has no VLD decode entrypoint for VAProfile {profile}");
        }
        Ok(())
    }

    /// Every entrypoint the driver offers for `profile`.
    pub fn entrypoints(&self, profile: c_int) -> Result<Vec<c_int>> {
        // SAFETY: `vaMaxNumEntrypoints` returns the array size this display needs;
        // the vector is allocated to exactly that and `count` is a local written
        // through by the call.
        unsafe {
            let max = (self.va.max_entrypoints)(self.display);
            if max <= 0 {
                bail!("vaMaxNumEntrypoints returned {max}");
            }
            let mut entrypoints = vec![0 as c_int; max as usize];
            let mut count: c_int = 0;
            self.va.check(
                "vaQueryConfigEntrypoints",
                (self.va.query_config_entrypoints)(
                    self.display,
                    profile,
                    entrypoints.as_mut_ptr(),
                    &mut count,
                ),
            )?;
            entrypoints.truncate(count.clamp(0, max) as usize);
            Ok(entrypoints)
        }
    }

    /// libva copies a non-null `data` before returning, so the caller's structs may
    /// die immediately. `size` is one element and `count` is how many — not
    /// interchangeable. H.264/H.265 pass `count = 1`; AV1's tile-parameter buffer
    /// is the exception (one buffer, a whole tile group's records).
    pub fn create_buffer(
        &self,
        context: VaContextId,
        kind: u32,
        size: usize,
        count: usize,
        data: *const c_void,
    ) -> Result<VaBufferId> {
        let mut id: VaBufferId = VA_INVALID_ID;
        // SAFETY: a live display and context; `data` points at `size * count` readable
        // bytes for the duration of the call (the caller's live struct or slice), and
        // `id` is a local written through. libva copies the payload before returning.
        self.va.check("vaCreateBuffer", unsafe {
            (self.va.create_buffer)(
                self.display,
                context,
                kind as c_uint,
                size as c_uint,
                count as c_uint,
                data.cast_mut(),
                &mut id,
            )
        })?;
        Ok(id)
    }

    /// `vaEndPicture` does not consume buffers. `va.h` requires `vaDestroyBuffer`;
    /// leaking two-plus per picture at 60 fps exhausts the driver's store.
    pub fn destroy_buffers(&self, buffers: &[VaBufferId]) {
        for &b in buffers {
            if b == VA_INVALID_ID {
                continue;
            }
            // SAFETY: each id came from `create_buffer` on this display and is
            // destroyed exactly once — the submission's list is consumed here.
            unsafe { (self.va.destroy_buffer)(self.display, b) };
        }
    }

    /// `vaCreateConfig`; libva copies `attribs` before returning. An empty slice
    /// passes null, as VideoProc's attribute-less config does.
    pub fn create_config(
        &self,
        profile: c_int,
        entrypoint: c_int,
        attribs: &[VaConfigAttrib],
    ) -> Result<VaObject<'_>> {
        let list = if attribs.is_empty() {
            std::ptr::null_mut()
        } else {
            attribs.as_ptr().cast_mut().cast::<c_void>()
        };
        let mut id: VaConfigId = VA_INVALID_ID;
        // SAFETY: a live display; `list` is null with a zero count or `attribs` with
        // its length, which the driver only reads; `id` is a local written through.
        self.va.check("vaCreateConfig", unsafe {
            (self.va.create_config)(
                self.display,
                profile,
                entrypoint,
                list,
                attribs.len() as c_int,
                &mut id,
            )
        })?;
        Ok(VaObject {
            display: self,
            id,
            destroy: self.va.destroy_config,
        })
    }

    /// `vaCreateContext` over `targets`, which libva copies; an empty slice pins
    /// none, as VideoProc wants. The surfaces stay the caller's to destroy.
    pub fn create_context(
        &self,
        config: VaConfigId,
        width: u32,
        height: u32,
        targets: &mut [VaSurfaceId],
    ) -> Result<VaObject<'_>> {
        let pinned = if targets.is_empty() {
            std::ptr::null_mut()
        } else {
            targets.as_mut_ptr()
        };
        let mut id: VaContextId = VA_INVALID_ID;
        // SAFETY: a live display and a config created on it; `pinned` is null with a
        // zero count or `targets` with its length; `id` is a local written through.
        self.va.check("vaCreateContext", unsafe {
            (self.va.create_context)(
                self.display,
                config,
                width as c_int,
                height as c_int,
                VA_PROGRESSIVE as c_int,
                pinned,
                targets.len() as c_int,
                &mut id,
            )
        })?;
        Ok(VaObject {
            display: self,
            id,
            destroy: self.va.destroy_context,
        })
    }

    /// One surface of `fourcc` in an `rt_format` pool; `None` lets the driver pick.
    pub fn create_surface(
        &self,
        rt_format: u32,
        fourcc: Option<u32>,
        width: u32,
        height: u32,
    ) -> Result<VaSurfaceId> {
        let mut attrs = Vec::new();
        if let Some(fourcc) = fourcc {
            attrs.push(VaSurfaceAttrib {
                kind: VA_SURFACE_ATTRIB_PIXEL_FORMAT,
                flags: VA_SURFACE_ATTRIB_SETTABLE,
                // Integer arm is i32; every fourcc here has the top bit clear.
                value: VaGenericValue::integer(fourcc as i32),
            });
        }
        self.create_surface_with(rt_format, width, height, &mut attrs)
    }

    fn create_surface_with(
        &self,
        rt_format: u32,
        width: u32,
        height: u32,
        attrs: &mut [VaSurfaceAttrib],
    ) -> Result<VaSurfaceId> {
        let mut id = VA_INVALID_ID;
        // SAFETY: live display; `attrs` outlives the call and the count is its
        // length; `id` is a local written through.
        self.va.check("vaCreateSurfaces", unsafe {
            (self.va.create_surfaces)(
                self.display,
                rt_format,
                width,
                height,
                &mut id,
                1,
                attrs.as_mut_ptr(),
                attrs.len() as c_uint,
            )
        })?;
        Ok(id)
    }

    pub fn destroy_surface(&self, surface: VaSurfaceId) {
        let mut ids = [surface];
        // SAFETY: created on this display, destroyed once.
        unsafe { (self.va.destroy_surfaces)(self.display, ids.as_mut_ptr(), 1) };
    }

    /// Wrap a capture dmabuf as a surface. libva takes its own reference on each
    /// fd, so the caller's stay theirs; destroy the surface like any other.
    pub fn import_dmabuf(&self, source: &DmabufSource) -> Result<VaSurfaceId> {
        let (fourcc, rt_format) = pf_vaapi::vpp::import_format(source.drm_fourcc)
            .ok_or_else(|| anyhow!("no ingest for DRM fourcc {:#x}", source.drm_fourcc))?;
        let mut desc = VaDrmPrimeSurfaceDescriptor::zeroed();
        desc.fourcc = fourcc;
        desc.width = source.width;
        desc.height = source.height;
        // One object per distinct fd, in first-seen order; every plane indexes one.
        let mut layer = desc.layers[0];
        layer.drm_format = source.drm_fourcc;
        for (i, plane) in source.planes.iter().enumerate() {
            if i >= MAX_PLANES_PER_LAYER {
                bail!("a dmabuf of {} planes", source.planes.len());
            }
            let objects = &mut desc.objects[..];
            let n = desc.num_objects as usize;
            let object = match objects[..n].iter().position(|o| o.fd == plane.fd) {
                Some(o) => o,
                None if n < MAX_OBJECTS => {
                    // SAFETY: the plane fds are the caller's, open for this call
                    // (`DmabufSource`'s contract); the borrow ends with the statement.
                    let fd = unsafe { std::os::fd::BorrowedFd::borrow_raw(plane.fd) };
                    objects[n] = VaDrmPrimeObject {
                        fd: plane.fd,
                        size: dmabuf_size(fd)?,
                        drm_format_modifier: source.modifier,
                    };
                    desc.num_objects += 1;
                    n
                }
                None => bail!("a dmabuf over more than {MAX_OBJECTS} fds"),
            };
            layer.object_index[i] = object as u32;
            layer.offset[i] = plane.offset;
            layer.pitch[i] = plane.stride;
            layer.num_planes += 1;
        }
        desc.layers[0] = layer;
        desc.num_layers = 1;

        let mut attrs = [
            VaSurfaceAttrib {
                kind: VA_SURFACE_ATTRIB_MEMORY_TYPE,
                flags: VA_SURFACE_ATTRIB_SETTABLE,
                value: VaGenericValue::integer(VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2 as i32),
            },
            VaSurfaceAttrib {
                kind: VA_SURFACE_ATTRIB_EXTERNAL_BUFFER_DESCRIPTOR,
                flags: VA_SURFACE_ATTRIB_SETTABLE,
                value: VaGenericValue::pointer((&raw mut desc).cast::<c_void>()),
            },
        ];
        // `desc` outlives the call inside `create_surface_with`; libva reads it there.
        self.create_surface_with(rt_format, source.width, source.height, &mut attrs)
    }

    /// Derive an image over `surface`, map it, hand `f` the layout and the pointer,
    /// then unmap and destroy on every path.
    pub fn map_image<T>(
        &self,
        surface: VaSurfaceId,
        f: impl FnOnce(&VaImage, *mut u8) -> Result<T>,
    ) -> Result<T> {
        // SAFETY: `VaImage` is all-integer `repr(C)` with no niche, so an all-zero
        // value is valid; `vaDeriveImage` overwrites every field it defines.
        let mut image: VaImage = unsafe { std::mem::zeroed() };
        // SAFETY: `image` is a local the call fills; `surface` is on this display.
        self.va.check("vaDeriveImage", unsafe {
            (self.va.derive_image)(self.display, surface, (&raw mut image).cast::<c_void>())
        })?;
        let mut ptr: *mut c_void = std::ptr::null_mut();
        // SAFETY: `image.buf` is the derived image's buffer on this display and
        // `ptr` is written through. Unmapped below on every path.
        let mapped = self.va.check("vaMapBuffer(image)", unsafe {
            (self.va.map_buffer)(self.display, image.buf, &mut ptr)
        });
        let result = mapped.and_then(|()| {
            if ptr.is_null() {
                bail!("vaMapBuffer returned success with a null pointer");
            }
            let value = f(&image, ptr.cast::<u8>());
            // SAFETY: the buffer that was mapped, unmapped once.
            let unmap = unsafe { (self.va.unmap_buffer)(self.display, image.buf) };
            let value = value?;
            self.va.check("vaUnmapBuffer(image)", unmap)?;
            Ok(value)
        });
        // SAFETY: `image_id` came from the `vaDeriveImage` above and is destroyed
        // exactly once, after the mapping is gone.
        let destroy = unsafe { (self.va.destroy_image)(self.display, image.image_id) };
        let value = result?;
        self.va.check("vaDestroyImage", destroy)?;
        Ok(value)
    }

    /// Copy one packed plane into `surface`, row by row, at the driver's pitch.
    pub fn write_packed(&self, surface: VaSurfaceId, bytes: &[u8], row_bytes: usize) -> Result<()> {
        self.map_image(surface, |image, ptr| {
            let rows = usize::from(image.height);
            let pitch = image.pitches[0] as usize;
            if bytes.len() < rows * row_bytes {
                bail!(
                    "{} source bytes for {rows} rows of {row_bytes}",
                    bytes.len()
                );
            }
            if row_bytes > pitch {
                bail!("rows of {row_bytes} bytes do not fit the surface pitch {pitch}");
            }
            for row in 0..rows {
                // SAFETY: the mapped image is at least `data_size` bytes and the
                // driver's own pitch and offset bound every row written; the source
                // was length-checked above and `row_bytes <= pitch`.
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        bytes.as_ptr().add(row * row_bytes),
                        ptr.add(image.offsets[0] as usize + row * pitch),
                        row_bytes,
                    );
                }
            }
            Ok(())
        })
    }
}

/// A config or context destroyed on drop, so a constructor's early return frees it
/// even on a display that outlives the constructor. [`Self::keep`] hands the id to
/// an owner that destroys it in libva's order.
#[must_use]
pub struct VaObject<'d> {
    display: &'d Display,
    id: c_uint,
    destroy: unsafe extern "C" fn(VaDisplay, c_uint) -> VaStatus,
}

impl VaObject<'_> {
    pub fn id(&self) -> c_uint {
        self.id
    }

    pub fn keep(self) -> c_uint {
        let id = self.id;
        std::mem::forget(self);
        id
    }
}

impl Drop for VaObject<'_> {
    fn drop(&mut self) {
        // SAFETY: `id` was created on this display by the call that chose `destroy`,
        // and `keep` forgets the guard, so this runs at most once per id.
        unsafe { (self.destroy)(self.display.display, self.id) };
    }
}

/// One capture dmabuf: DRM fourcc, tiling modifier, and each plane's fd, offset and
/// stride. The fds are borrowed for the call.
#[derive(Clone, Copy, Debug)]
pub struct DmabufSource<'a> {
    pub width: u32,
    pub height: u32,
    pub drm_fourcc: u32,
    pub modifier: u64,
    pub planes: &'a [ExportedPlane],
}

/// A dma-buf reports its size as its file size.
fn dmabuf_size(fd: std::os::fd::BorrowedFd<'_>) -> Result<u32> {
    let len = rustix::fs::fstat(fd)
        .with_context(|| format!("fstat dmabuf fd {}", fd.as_raw_fd()))?
        .st_size;
    u32::try_from(len).context("a dmabuf over 4 GiB")
}

impl Drop for Display {
    fn drop(&mut self) {
        // SAFETY: `self.display` was initialised in `open_node` and nothing else
        // terminates it; `Drop` runs once. The node fd is dropped AFTER this, which is
        // the order libva requires — it holds the fd, it does not own it.
        unsafe { (self.va.terminate)(self.display) };
        self.node = None;
    }
}
