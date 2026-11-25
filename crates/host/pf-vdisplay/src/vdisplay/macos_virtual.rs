//! PRIVATE API. `CGVirtualDisplay`, `CGVirtualDisplayDescriptor`, `CGVirtualDisplaySettings`
//! and `CGVirtualDisplayMode` are undocumented CoreGraphics classes, and this is the only file
//! that touches them. Each class is looked up through the Objective-C runtime at run time and
//! messaged through `objc2`; no private symbol is linked, so the host still loads on a macOS
//! that drops them. [`available`] is the presence check: it also verifies every selector's
//! types, and nothing here runs until it has answered `true`.
//!
//! Selector names and property types follow Chromium's `virtual_display_mac_util.mm` and
//! DeskPad's headers, checked against the runtime of macOS 27. The mode initializer that takes
//! a transfer function is in neither: it is read off that runtime, and [`hdr_modes`] checks it.

use anyhow::{bail, Context, Result};
use objc2::rc::{autoreleasepool, Allocated, Retained};
use objc2::runtime::{AnyClass, AnyObject, Bool, Sel};
use objc2::{msg_send, sel};
use objc2_core_foundation::{CGPoint, CGSize};
use objc2_foundation::{NSArray, NSString};
use std::ffi::CStr;
use std::sync::OnceLock;

const DISPLAY: &CStr = c"CGVirtualDisplay";
const DESCRIPTOR: &CStr = c"CGVirtualDisplayDescriptor";
const SETTINGS: &CStr = c"CGVirtualDisplaySettings";
const MODE: &CStr = c"CGVirtualDisplayMode";

/// What a new display is: its pixel size, rate and the identity macOS keys its arrangement on.
pub(crate) struct Spec<'a> {
    pub name: &'a str,
    pub width: u32,
    pub height: u32,
    pub refresh_hz: f64,
    pub vendor: u32,
    pub product: u32,
    pub serial: u32,
    /// Its modes are HDR: macOS gives the display headroom above white. Needs [`hdr_modes`].
    pub hdr: bool,
    /// The size the picture has on the client's glass, in millimetres: the panel this
    /// display declares. `None` sizes the mode at [`DPI`].
    pub mm: Option<(u16, u16)>,
}

/// Whether this macOS makes HDR displays: the mode initializer that takes a transfer function,
/// with the types [`Display::apply`] sends.
pub(crate) fn hdr_modes() -> bool {
    static YES: OnceLock<bool> = OnceLock::new();
    *YES.get_or_init(|| {
        let sel = sel!(initWithWidth:height:refreshRate:transferFunction:);
        available()
            && AnyClass::get(MODE).is_some_and(|c| {
                (c.verify_sel::<(u32, u32, f64, u32), *mut AnyObject>(sel)).is_ok()
            })
    })
}

/// Whether this macOS has the four classes, each method this file sends with the types it sends.
/// Read once; logs once when not.
pub(crate) fn available() -> bool {
    static PRESENT: OnceLock<bool> = OnceLock::new();
    *PRESENT.get_or_init(|| match missing() {
        None => true,
        Some(what) => {
            tracing::warn!(
                missing = %what,
                "this macOS offers no virtual displays: every session mirrors an existing display"
            );
            false
        }
    })
}

type Check = (&'static CStr, Sel, fn(&AnyClass, Sel) -> bool);

/// The first class or method [`available`] lacks, or `None`.
fn missing() -> Option<String> {
    let checks: [Check; 20] = [
        (DESCRIPTOR, sel!(init), |c, s| {
            c.verify_sel::<(), *mut AnyObject>(s).is_ok()
        }),
        (DESCRIPTOR, sel!(setName:), |c, s| {
            c.verify_sel::<(&AnyObject,), ()>(s).is_ok()
        }),
        (DESCRIPTOR, sel!(setMaxPixelsWide:), |c, s| {
            c.verify_sel::<(u32,), ()>(s).is_ok()
        }),
        (DESCRIPTOR, sel!(setMaxPixelsHigh:), |c, s| {
            c.verify_sel::<(u32,), ()>(s).is_ok()
        }),
        (DESCRIPTOR, sel!(setSizeInMillimeters:), |c, s| {
            c.verify_sel::<(CGSize,), ()>(s).is_ok()
        }),
        (DESCRIPTOR, sel!(setVendorID:), |c, s| {
            c.verify_sel::<(u32,), ()>(s).is_ok()
        }),
        (DESCRIPTOR, sel!(setProductID:), |c, s| {
            c.verify_sel::<(u32,), ()>(s).is_ok()
        }),
        (DESCRIPTOR, sel!(setSerialNum:), |c, s| {
            c.verify_sel::<(u32,), ()>(s).is_ok()
        }),
        (DESCRIPTOR, sel!(setQueue:), |c, s| {
            c.verify_sel::<(&AnyObject,), ()>(s).is_ok()
        }),
        (DESCRIPTOR, sel!(setRedPrimary:), |c, s| {
            c.verify_sel::<(CGPoint,), ()>(s).is_ok()
        }),
        (DESCRIPTOR, sel!(setGreenPrimary:), |c, s| {
            c.verify_sel::<(CGPoint,), ()>(s).is_ok()
        }),
        (DESCRIPTOR, sel!(setBluePrimary:), |c, s| {
            c.verify_sel::<(CGPoint,), ()>(s).is_ok()
        }),
        (DESCRIPTOR, sel!(setWhitePoint:), |c, s| {
            c.verify_sel::<(CGPoint,), ()>(s).is_ok()
        }),
        (DISPLAY, sel!(initWithDescriptor:), |c, s| {
            c.verify_sel::<(&AnyObject,), *mut AnyObject>(s).is_ok()
        }),
        (DISPLAY, sel!(applySettings:), |c, s| {
            c.verify_sel::<(&AnyObject,), Bool>(s).is_ok()
        }),
        (DISPLAY, sel!(displayID), |c, s| {
            c.verify_sel::<(), u32>(s).is_ok()
        }),
        (SETTINGS, sel!(init), |c, s| {
            c.verify_sel::<(), *mut AnyObject>(s).is_ok()
        }),
        (SETTINGS, sel!(setHiDPI:), |c, s| {
            c.verify_sel::<(u32,), ()>(s).is_ok()
        }),
        (SETTINGS, sel!(setModes:), |c, s| {
            c.verify_sel::<(&AnyObject,), ()>(s).is_ok()
        }),
        (MODE, sel!(initWithWidth:height:refreshRate:), |c, s| {
            c.verify_sel::<(u32, u32, f64), *mut AnyObject>(s).is_ok()
        }),
    ];
    for (class, sel, ok) in checks {
        let Some(cls) = AnyClass::get(class) else {
            return Some(class.to_string_lossy().into_owned());
        };
        if !ok(cls, sel) {
            let (class, sel) = (class.to_string_lossy(), sel.name().to_string_lossy());
            return Some(format!("{class} {sel}"));
        }
    }
    None
}

/// One virtual display. Dropping it removes the display.
pub(crate) struct Display {
    object: Option<Retained<AnyObject>>,
    id: u32,
    /// The size it was made at, the largest it takes.
    max: (u32, u32),
    /// Every mode it takes is HDR.
    hdr: bool,
}

// SAFETY: `Display` owns its object alone and reaches it only through borrows the caller
// serializes (the backend keeps it under a `Mutex`). The object keeps no thread-bound state:
// its callbacks run on the dispatch queue its descriptor names.
unsafe impl Send for Display {}

impl Display {
    /// Make a display at `spec`'s mode. The caller checked [`available`]. macOS brings it online
    /// beside the others a moment after this returns.
    pub(crate) fn create(spec: &Spec) -> Result<Display> {
        autoreleasepool(|_| {
            let descriptor = new(DESCRIPTOR).context("make the display descriptor")?;
            let name = NSString::from_str(spec.name);
            let queue = dispatch2::DispatchQueue::global_queue(
                dispatch2::GlobalQueueIdentifier::QualityOfService(
                    dispatch2::DispatchQoS::UserInitiated,
                ),
            );
            let (w, h) = panel_mm(spec.width, spec.height, spec.mm);
            let size = CGSize::new(w, h);
            let d = &*descriptor;
            // SAFETY: setters on a descriptor this function owns, each verified by `available`
            // to take exactly the type passed. The setters retain the name and the queue.
            unsafe {
                let _: () = msg_send![d, setName: &*name];
                let _: () = msg_send![d, setMaxPixelsWide: spec.width];
                let _: () = msg_send![d, setMaxPixelsHigh: spec.height];
                let _: () = msg_send![d, setSizeInMillimeters: size];
                let _: () = msg_send![d, setVendorID: spec.vendor];
                let _: () = msg_send![d, setProductID: spec.product];
                let _: () = msg_send![d, setSerialNum: spec.serial];
                let _: () = msg_send![d, setQueue: &*queue];
                let _: () = msg_send![d, setRedPrimary: SRGB[0]];
                let _: () = msg_send![d, setGreenPrimary: SRGB[1]];
                let _: () = msg_send![d, setBluePrimary: SRGB[2]];
                let _: () = msg_send![d, setWhitePoint: SRGB[3]];
            }
            let class = AnyClass::get(DISPLAY).context("find CGVirtualDisplay")?;
            // SAFETY: `alloc` on a class `available` found; `initWithDescriptor:` takes the
            // allocation and a live descriptor, as verified, and answers nil on failure.
            let object: Option<Retained<AnyObject>> = unsafe {
                let raw: Allocated<AnyObject> = msg_send![class, alloc];
                msg_send![raw, initWithDescriptor: d]
            };
            let object = object.context("the window server refused the virtual display")?;
            // SAFETY: a plain getter on a live display, verified to answer a `u32`.
            let id: u32 = unsafe { msg_send![&*object, displayID] };
            let display = Display {
                object: Some(object),
                id,
                max: (spec.width, spec.height),
                hdr: spec.hdr && hdr_modes(),
            };
            if id == 0 {
                bail!("the virtual display has no display id");
            }
            if !display.apply(spec.width, spec.height, spec.refresh_hz)? {
                bail!("the virtual display refused its first mode");
            }
            Ok(display)
        })
    }

    pub(crate) fn id(&self) -> u32 {
        self.id
    }

    pub(crate) fn hdr(&self) -> bool {
        self.hdr
    }

    /// Set the display's one mode, one pixel to a point, HDR on an HDR display: `false` when
    /// macOS refuses it, or when the mode is past `max`, which macOS refuses by dropping the
    /// display to 1x1. One mode only: lists of two aborted the WindowServer, ending the desktop
    /// session (macOS 27.2).
    pub(crate) fn apply(&self, width: u32, height: u32, refresh_hz: f64) -> Result<bool> {
        let object = self.object.as_deref().context("the display is gone")?;
        if width > self.max.0 || height > self.max.1 {
            return Ok(false);
        }
        autoreleasepool(|_| {
            let mode_class = AnyClass::get(MODE).context("find CGVirtualDisplayMode")?;
            // SAFETY: `alloc` on a class `available` found; each initializer takes the
            // allocation and its verified types (`hdr` is set only where `hdr_modes` checked
            // the longer one), and answers nil on failure.
            let mode: Option<Retained<AnyObject>> = unsafe {
                let raw: Allocated<AnyObject> = msg_send![mode_class, alloc];
                if self.hdr {
                    msg_send![
                        raw,
                        initWithWidth: width,
                        height: height,
                        refreshRate: refresh_hz,
                        transferFunction: HDR_TRANSFER
                    ]
                } else {
                    msg_send![raw, initWithWidth: width, height: height, refreshRate: refresh_hz]
                }
            };
            let mode = mode.context("make the display mode")?;
            let settings = new(SETTINGS).context("make the display settings")?;
            let modes = NSArray::from_retained_slice(&[mode]);
            // SAFETY: setters on settings this function owns and `applySettings:` on a live
            // display, each verified to take the type passed. The setter retains `modes`.
            let applied: bool = unsafe {
                let _: () = msg_send![&*settings, setHiDPI: 0u32];
                let _: () = msg_send![&*settings, setModes: &*modes];
                msg_send![object, applySettings: &*settings]
            };
            Ok(applied)
        })
    }
}

impl Drop for Display {
    fn drop(&mut self) {
        // The last release removes the display; what it autoreleases drains here, not at
        // thread exit.
        autoreleasepool(|_| drop(self.object.take()));
    }
}

/// A desktop-size display reads as an ordinary monitor, not a high-density panel.
const DPI: f64 = 96.0;

/// The panel a `width`x`height` display declares, in millimetres: the client's `mm`, else
/// the mode at [`DPI`].
fn panel_mm(width: u32, height: u32, mm: Option<(u16, u16)>) -> (f64, f64) {
    let at_dpi = |px: u32| f64::from(px) * 25.4 / DPI;
    mm.map_or((at_dpi(width), at_dpi(height)), |(w, h)| {
        (w.into(), h.into())
    })
}

/// The transfer function of an HDR mode: the screen then reports headroom of 5 above white.
/// 0 and 2 to 6 make an SDR mode (macOS 27.2).
const HDR_TRANSFER: u32 = 1;

/// BT.709 primaries and D65 white: the colour an SDR capture of it converts to. An HDR capture
/// carries colour past them.
const SRGB: [CGPoint; 4] = [
    CGPoint { x: 0.64, y: 0.33 },
    CGPoint { x: 0.30, y: 0.60 },
    CGPoint { x: 0.15, y: 0.06 },
    CGPoint {
        x: 0.3127,
        y: 0.3290,
    },
];

/// `[[class alloc] init]` for a class [`available`] checked.
fn new(class: &CStr) -> Option<Retained<AnyObject>> {
    let class = AnyClass::get(class)?;
    // SAFETY: `alloc` then the verified `init` on a fresh allocation, which answers nil on
    // failure.
    unsafe {
        let raw: Allocated<AnyObject> = msg_send![class, alloc];
        msg_send![raw, init]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The client's size is the panel at every mode; an unknown one sizes the mode at 96 DPI.
    #[test]
    fn the_panel_size_is_the_clients() {
        assert_eq!(panel_mm(3840, 2160, Some((598, 336))), (598.0, 336.0));
        assert_eq!(panel_mm(1920, 1080, Some((598, 336))), (598.0, 336.0));
        assert_eq!(panel_mm(3840, 2160, None).0.round(), 1016.0);
    }

    /// The presence check reads the runtime alone and creates nothing.
    #[test]
    fn the_presence_check_agrees_with_what_is_missing() {
        match missing() {
            Some(what) => assert!(!what.is_empty() && !available()),
            None => assert!(available()),
        }
    }
}
