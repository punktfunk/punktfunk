//! macOS displays through CoreGraphics: the online heads a mirror streams, and [`OwnScreen`],
//! a session's own screen. That is a virtual display at the client's mode where this macOS
//! has one (`macos_virtual`, a private API behind a presence check), else a mirror of the main
//! display. A virtual display of ours lists as `managed`.
//!
//! A head's connector is its `CGDirectDisplayID` in decimal: what a pin names and what capture
//! opens. Geometry follows [`PhysicalMonitor`]: origin in global points, size in the current
//! mode's pixels, `scale` the ratio between them.
//!
//! Topology uses the public display configuration, scoped to this process
//! (`kCGConfigureForAppOnly`), so macOS takes it back if the host dies. `extend` leaves the
//! arrangement as macOS made it. `primary` moves the virtual display to the origin, the main
//! display's place, shifting every display by the same offset. `exclusive` makes every other
//! lit head a mirror of the virtual display, since no public call turns a panel off: those
//! panels show the stream's picture. Only the first live virtual display arranges; the rest
//! extend. Teardown puts the arrangement back before it removes the display.
//!
//! After its first display configuration, a process sees a display made later only while an
//! AppKit loop runs on its main thread; `punktfunk-host` runs one.

use crate::backend::{VirtualDisplay, VirtualOutput};
use crate::macos_virtual::{Display, Spec};
use crate::monitors::PhysicalMonitor;
use crate::policy::Topology;
use crate::{Compositor, Mode};
use anyhow::{bail, Context, Result};
use objc2_core_graphics::{
    kCGNullDirectDisplay, CGBeginDisplayConfiguration, CGCancelDisplayConfiguration,
    CGCompleteDisplayConfiguration, CGConfigureDisplayMirrorOfDisplay, CGConfigureDisplayOrigin,
    CGConfigureOption, CGDisplayBounds, CGDisplayConfigRef, CGDisplayCopyDisplayMode,
    CGDisplayIsActive, CGDisplayMirrorsDisplay, CGDisplayMode, CGDisplayModelNumber,
    CGDisplayPixelsHigh, CGDisplayPixelsWide, CGDisplayVendorNumber, CGError,
    CGGetActiveDisplayList, CGGetOnlineDisplayList, CGMainDisplayID,
};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

/// More heads than a Mac drives.
const MAX_DISPLAYS: usize = 32;
/// Vendor and product a virtual display of ours carries. `0x7fff` spells no EDID maker, so no
/// real panel reads as ours.
const VENDOR: u32 = 0x7fff;
const PRODUCT: u32 = 0x5046;
/// How long macOS gets to bring a display online or take it away.
const SETTLE: Duration = Duration::from_secs(3);
/// How long a released display may stay listed before [`Screen::retire`] unpins it. An
/// unconfigured one goes in about 100 ms.
const UNPINNED: Duration = Duration::from_millis(500);

/// Every online display, sorted by `(x, y, connector)`. One asleep or mirroring another is
/// listed disabled: it shows nothing of its own to capture.
pub(crate) fn list_monitors() -> Result<Vec<PhysicalMonitor>> {
    let main = CGMainDisplayID();
    let mut heads: Vec<_> = online()?.into_iter().map(|id| read(id, main)).collect();
    heads.sort_by(|a, b| (a.x, a.y, &a.connector).cmp(&(b.x, b.y, &b.connector)));
    Ok(heads)
}

/// The main display's connector: what a session mirrors where it gets no display of its own.
pub fn main_display() -> String {
    CGMainDisplayID().to_string()
}

/// Whether a session's own screen is a virtual display here: this macOS has the classes.
pub fn creates_displays() -> bool {
    crate::macos_virtual::available()
}

fn online() -> Result<Vec<u32>> {
    list(|max, ids, count| {
        // SAFETY: `ids` has room for `max` entries and `count` is a live local; the call
        // keeps neither pointer.
        unsafe { CGGetOnlineDisplayList(max, ids, count) }
    })
}

fn active() -> Result<Vec<u32>> {
    list(|max, ids, count| {
        // SAFETY: as in `online`.
        unsafe { CGGetActiveDisplayList(max, ids, count) }
    })
}

fn list(call: impl FnOnce(u32, *mut u32, &mut u32) -> CGError) -> Result<Vec<u32>> {
    let mut ids = [0u32; MAX_DISPLAYS];
    let mut count = 0u32;
    let err = call(MAX_DISPLAYS as u32, ids.as_mut_ptr(), &mut count);
    if err != CGError::Success {
        bail!("list displays: CGError {}", err.0);
    }
    Ok(ids[..(count as usize).min(MAX_DISPLAYS)].to_vec())
}

fn read(id: u32, main: u32) -> PhysicalMonitor {
    let origin = CGDisplayBounds(id).origin;
    let mode = CGDisplayCopyDisplayMode(id);
    // Without the AppKit loop, a process reads no mode for a display that came online after its
    // first display read. Its size still reads, as points.
    let (pixels, points_wide) = match mode.as_deref() {
        Some(m) => (
            (
                CGDisplayMode::pixel_width(Some(m)),
                CGDisplayMode::pixel_height(Some(m)),
            ),
            CGDisplayMode::width(Some(m)),
        ),
        None => (
            (CGDisplayPixelsWide(id), CGDisplayPixelsHigh(id)),
            CGDisplayPixelsWide(id),
        ),
    };
    let mut head = head(
        id,
        (origin.x, origin.y),
        pixels,
        points_wide,
        CGDisplayMode::refresh_rate(mode.as_deref()),
        id == main,
        CGDisplayIsActive(id),
    );
    head.managed = CGDisplayVendorNumber(id) == VENDOR && CGDisplayModelNumber(id) == PRODUCT;
    head
}

/// One head from its CoreGraphics reads. A display with no mode reads zero pixels and scale 1;
/// a panel that reports no rate reads 0 mHz.
fn head(
    id: u32,
    origin: (f64, f64),
    pixels: (usize, usize),
    points_wide: usize,
    refresh_hz: f64,
    primary: bool,
    enabled: bool,
) -> PhysicalMonitor {
    let connector = id.to_string();
    PhysicalMonitor {
        description: connector.clone(),
        connector,
        width: pixels.0 as u32,
        height: pixels.1 as u32,
        refresh_mhz: (refresh_hz * 1000.0).round() as u32,
        x: origin.0 as i32,
        y: origin.1 as i32,
        scale: if points_wide > 0 {
            pixels.0 as f64 / points_wide as f64
        } else {
            1.0
        },
        primary,
        enabled,
        managed: false,
    }
}

/// A session's own screen: a virtual display at the asked mode. A rebuild at the same mode keeps
/// the display it made; another mode changes it in place, or replaces it when the display
/// refuses the mode. A `join` session shares the oldest live one. A display that can't be made
/// falls back to a mirror of the main display, framed at the asked size.
#[derive(Default)]
pub(crate) struct OwnScreen {
    topology: Option<Topology>,
    client: Option<[u8; 32]>,
    slot: Option<u32>,
    join: bool,
    live: Weak<Screen>,
}

impl VirtualDisplay for OwnScreen {
    fn name(&self) -> &'static str {
        "macos"
    }

    fn set_client_identity(&mut self, fingerprint: Option<[u8; 32]>) {
        self.client = fingerprint;
    }

    fn set_topology(&mut self, topology: Option<Topology>) {
        self.topology = topology;
    }

    fn topology(&self) -> Option<Topology> {
        self.topology
    }

    fn last_identity_slot(&self) -> Option<u32> {
        self.slot
    }

    fn set_join_live(&mut self, on: bool) {
        self.join = on;
    }

    fn join_live(&self) -> bool {
        self.join
    }

    fn create(&mut self, mode: Mode) -> Result<VirtualOutput> {
        if let Some(screen) = self.live.upgrade() {
            let asked = (mode.width, mode.height, mode.refresh_hz);
            if let Some(l) = live().into_iter().find(|l| l.entry == screen.entry) {
                if l.mode == asked {
                    return Ok(output(screen, l.id, mode.millihz()));
                }
                let start = Instant::now();
                match screen.change(mode) {
                    Ok(()) => {
                        tracing::info!(
                            display_id = l.id,
                            mode = ?asked,
                            ms = start.elapsed().as_millis() as u64,
                            "virtual display changed mode in place"
                        );
                        return Ok(output(screen, l.id, mode.millihz()));
                    }
                    Err(e) => tracing::info!(
                        mode = ?asked,
                        error = %format!("{e:#}"),
                        "making the virtual display again at the new mode"
                    ),
                }
            }
            // macOS refuses a second display with the same identity: the old one goes first.
            screen.retire();
        }
        if self.join
            && let Some((screen, l)) =
                (live().into_iter()).find_map(|l| Some((l.screen.upgrade()?, l)))
        {
            tracing::info!(display_id = l.id, "sharing the live virtual display");
            return Ok(output(screen, l.id, l.mode.2.saturating_mul(1000)));
        }
        match self.make(mode) {
            Ok(out) => Ok(out),
            Err(e) => {
                tracing::warn!(
                    error = %format!("{e:#}"),
                    "virtual display not created: mirroring the main display instead"
                );
                let mut mirror =
                    crate::mirror::MirrorDisplay::new(Compositor::MacOs, main_display())?;
                mirror.set_client_identity(self.client);
                let mut out = mirror.create(mode)?;
                // ScreenCaptureKit fits the display into the asked size, as a virtual display
                // would have delivered it.
                if let Some((_, _, hz)) = out.preferred_mode {
                    out.preferred_mode = Some((mode.width, mode.height, hz));
                }
                Ok(out)
            }
        }
    }
}

impl OwnScreen {
    fn make(&mut self, mode: Mode) -> Result<VirtualOutput> {
        let (width, height) = (mode.width, mode.height);
        self.slot = crate::identity::resolve_slot(
            self.client,
            (width, height),
            crate::policy::Identity::Shared,
        );
        let taken: Vec<u32> = live().iter().map(|l| l.serial).collect();
        let serial = serial(self.slot, mode, &taken);
        let display = Display::create(&Spec {
            name: "Punktfunk",
            width,
            height,
            refresh_hz: f64::from(mode.millihz()) / 1000.0,
            vendor: VENDOR,
            product: PRODUCT,
            serial,
        })?;
        let id = display.id();
        settle(SETTLE, "come online", || {
            let head = read(id, 0);
            head.enabled && (head.width, head.height) == (width, height)
        })
        .with_context(|| {
            let head = read(id, 0);
            let listed = online().unwrap_or_default();
            format!(
                "display {id} reads {}x{} active={} listed={listed:?}",
                head.width, head.height, head.enabled
            )
        })?;
        let topology = crate::resolve_topology(crate::create_topology(self.topology, self.client));
        let restore = if live().is_empty() {
            arrange(topology, id)
        } else {
            tracing::info!(
                display_id = id,
                ?topology,
                "another virtual display arranged the desktop: this one extends"
            );
            None
        };
        let screen = Screen::new(display, restore, mode, self.slot, serial);
        let label = format!("{width}x{height}@{}", mode.refresh_hz);
        tracing::info!(display_id = id, mode = %label, ?topology, "virtual display created");
        crate::emit_display_event(crate::DisplayEvent::Created {
            backend: self.name().into(),
            width,
            height,
            refresh_hz: mode.refresh_hz,
        });
        self.live = Arc::downgrade(&screen);
        Ok(output(screen, id, mode.millihz()))
    }
}

/// The output streaming display `id`, at the rate it reads back, else `asked_mhz`, the rate it
/// was set to.
fn output(screen: Arc<Screen>, id: u32, asked_mhz: u32) -> VirtualOutput {
    let head = read(id, 0);
    let mhz = Some(head.refresh_mhz)
        .filter(|&m| m > 0)
        .unwrap_or(asked_mhz);
    let refresh_hz = mhz.saturating_add(500) / 1000;
    let mut out = VirtualOutput::owned(
        0,
        Some((head.width, head.height, refresh_hz)),
        Box::new(screen),
    );
    out.refresh_mhz = Some(mhz).filter(|&m| m > 0);
    out.cg_display = Some(id);
    out
}

/// The serial a display of `mode` for identity `slot` carries. macOS brings a display back at
/// the mode it last ran under its identity whenever the new mode list holds it, so the mode is
/// part of the identity. Live displays hold the `taken` ones: macOS refuses a repeat, so of two
/// sessions making the same serial at once, one falls back to the mirror.
fn serial(slot: Option<u32>, mode: Mode, taken: &[u32]) -> u32 {
    let key = [slot.unwrap_or(0), mode.width, mode.height, mode.millihz()];
    // FNV-1a: the same serial for the same key on every host build.
    let hash = (key.iter().flat_map(|k| k.to_le_bytes())).fold(0x811c_9dc5u32, |h, b| {
        (h ^ u32::from(b)).wrapping_mul(0x0100_0193)
    });
    let mut serial = hash.max(1);
    while taken.contains(&serial) {
        serial = serial.wrapping_add(1).max(1);
    }
    serial
}

/// Poll `done` until it holds, for at most `limit`.
fn settle(limit: Duration, what: &str, done: impl Fn() -> bool) -> Result<()> {
    let start = Instant::now();
    while !done() {
        if start.elapsed() >= limit {
            bail!("the display did not {what} within {limit:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Ok(())
}

fn listed(id: u32) -> bool {
    online().is_ok_and(|ids| ids.contains(&id))
}

/// One live virtual display, held by every output that streams it.
struct Screen {
    display: Mutex<Option<Display>>,
    restore: Mutex<Option<Restore>>,
    entry: u64,
}

impl Screen {
    /// The display, listed live until its last holder drops it.
    fn new(
        display: Display,
        restore: Option<Restore>,
        mode: Mode,
        slot: Option<u32>,
        serial: u32,
    ) -> Arc<Screen> {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let entry = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let id = display.id();
        let screen = Arc::new(Screen {
            display: Mutex::new(Some(display)),
            restore: Mutex::new(restore),
            entry,
        });
        lock_live().push(Live {
            entry,
            id,
            mode: (mode.width, mode.height, mode.refresh_hz),
            slot,
            serial,
            screen: Arc::downgrade(&screen),
        });
        screen
    }

    /// Change the display to `mode` where it stands, then mirror again each head the change
    /// let go.
    fn change(&self, mode: Mode) -> Result<()> {
        let display = self.display.lock().unwrap_or_else(|e| e.into_inner());
        let display = display.as_ref().context("the display is gone")?;
        let (id, width, height) = (display.id(), mode.width, mode.height);
        if !display.apply(width, height, f64::from(mode.millihz()) / 1000.0)? {
            bail!("the display refused {width}x{height}");
        }
        settle(SETTLE, "take the mode", || {
            let head = read(id, 0);
            (head.width, head.height) == (width, height)
        })?;
        if let Some(restore) = self
            .restore
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            restore.hold(id);
        }
        for l in lock_live().iter_mut().filter(|l| l.entry == self.entry) {
            l.mode = (width, height, mode.refresh_hz);
        }
        Ok(())
    }

    /// Put the arrangement back and remove the display now, while outputs may still hold the
    /// screen. Once only; the last drop calls it too.
    fn retire(&self) {
        let restore = self
            .restore
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(restore) = restore {
            restore.run();
        }
        let display = self
            .display
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        let Some(display) = display else {
            return;
        };
        let id = display.id();
        drop(display);
        lock_live().retain(|l| l.entry != self.entry);
        // A configuration that named the display keeps it online after its release, until a
        // later one names it again. Checked on macOS 27.2.
        if settle(UNPINNED, "go away", || !listed(id)).is_err() {
            let _ = configure(|cfg| {
                // SAFETY: `cfg` is the open configuration `configure` passes in.
                let _ = unsafe { CGConfigureDisplayMirrorOfDisplay(cfg, id, kCGNullDirectDisplay) };
                Ok(())
            });
        }
        match settle(SETTLE, "go away", || !listed(id)) {
            Ok(()) => tracing::info!(display_id = id, "virtual display removed"),
            Err(e) => {
                let error = format!("{e:#}");
                tracing::warn!(display_id = id, %error, "virtual display still listed after release");
            }
        }
        crate::emit_display_event(crate::DisplayEvent::Released { count: 1 });
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        self.retire();
    }
}

/// A live virtual display, as `/display/state` lists it.
#[derive(Clone)]
struct Live {
    entry: u64,
    id: u32,
    mode: (u32, u32, u32),
    slot: Option<u32>,
    serial: u32,
    screen: Weak<Screen>,
}

static LIVE: Mutex<Vec<Live>> = Mutex::new(Vec::new());

fn lock_live() -> std::sync::MutexGuard<'static, Vec<Live>> {
    LIVE.lock().unwrap_or_else(|e| e.into_inner())
}

fn live() -> Vec<Live> {
    lock_live().clone()
}

/// The live virtual displays for `/display/state`: active, each its session's alone.
pub(crate) fn snapshot() -> Vec<crate::registry::DisplayInfo> {
    let topology = crate::registry::topology_str();
    live()
        .into_iter()
        .enumerate()
        .map(|(idx, l)| {
            let origin = CGDisplayBounds(l.id).origin;
            crate::registry::DisplayInfo {
                slot: l.entry,
                backend: "macos".into(),
                mode: l.mode,
                state: "active".into(),
                expires_in_ms: None,
                sessions: 1,
                client: None,
                group: 1,
                display_index: idx as u32,
                position: (origin.x as i32, origin.y as i32),
                scale: 1.0,
                identity_slot: l.slot,
                topology: topology.clone(),
            }
        })
        .collect()
}

/// What [`arrange`] changed: every active display's origin before it, and the heads it made
/// mirrors.
struct Restore {
    origins: Vec<(u32, i32, i32)>,
    mirrors: Vec<u32>,
}

impl Restore {
    /// Mirror `ours` again on each head that no longer does. macOS ends the mirror when ours
    /// takes the mode a head runs (macOS 27.2).
    fn hold(&self, ours: u32) {
        let lapsed: Vec<u32> = (self.mirrors.iter().copied())
            .filter(|&head| CGDisplayMirrorsDisplay(head) != ours)
            .collect();
        if lapsed.is_empty() {
            return;
        }
        let result = configure(|cfg| {
            for &head in &lapsed {
                // SAFETY: `cfg` is the open configuration `configure` passes in.
                check(unsafe { CGConfigureDisplayMirrorOfDisplay(cfg, head, ours) })?;
            }
            Ok(())
        });
        match result {
            Ok(()) => {
                tracing::info!(display_id = ours, mirrors = ?lapsed, "display arrangement held")
            }
            Err(e) => tracing::warn!(error = %format!("{e:#}"), "display arrangement not held"),
        }
    }

    /// Unmirror the heads still mirroring and put every display still listed back where it was.
    fn run(self) {
        let listed = online().unwrap_or_default();
        let result = configure(|cfg| {
            let mirroring = |h: &&u32| CGDisplayMirrorsDisplay(**h) != kCGNullDirectDisplay;
            for &head in self
                .mirrors
                .iter()
                .filter(|h| listed.contains(h))
                .filter(mirroring)
            {
                // SAFETY: `cfg` is the open configuration `configure` passes in.
                check(unsafe {
                    CGConfigureDisplayMirrorOfDisplay(cfg, head, kCGNullDirectDisplay)
                })?;
            }
            for &(id, x, y) in self.origins.iter().filter(|o| listed.contains(&o.0)) {
                // SAFETY: as above.
                check(unsafe { CGConfigureDisplayOrigin(cfg, id, x, y) })?;
            }
            Ok(())
        });
        match result {
            Ok(()) => tracing::info!(mirrors = ?self.mirrors, "display arrangement restored"),
            Err(e) => tracing::warn!(error = %format!("{e:#}"), "display arrangement not restored"),
        }
    }
}

/// Apply `topology` around the virtual display `ours`. A failure logs and leaves it extended.
fn arrange(topology: Topology, ours: u32) -> Option<Restore> {
    let origins = || -> Result<Vec<(u32, i32, i32)>> {
        Ok(active()?
            .into_iter()
            .map(|id| {
                let o = CGDisplayBounds(id).origin;
                (id, o.x as i32, o.y as i32)
            })
            .collect())
    };
    let applied = match topology {
        Topology::Extend | Topology::Auto => return None,
        Topology::Primary => origins().and_then(|before| {
            let after =
                primary_origins(&before, ours).context("the virtual display is not active")?;
            configure(|cfg| {
                for &(id, x, y) in &after {
                    // SAFETY: `cfg` is the open configuration `configure` passes in.
                    check(unsafe { CGConfigureDisplayOrigin(cfg, id, x, y) })?;
                }
                Ok(())
            })?;
            Ok(Restore {
                origins: before,
                mirrors: Vec::new(),
            })
        }),
        Topology::Exclusive => origins().and_then(|before| {
            let keep = crate::policy::prefs().get().keep_monitors;
            let mirrors = exclusive_mirrors(&list_monitors()?, ours, &keep);
            configure(|cfg| {
                for &head in &mirrors {
                    // SAFETY: `cfg` is the open configuration `configure` passes in.
                    check(unsafe { CGConfigureDisplayMirrorOfDisplay(cfg, head, ours) })?;
                }
                Ok(())
            })?;
            Ok(Restore {
                origins: before,
                mirrors,
            })
        }),
    };
    match applied {
        Ok(restore) => {
            let mirrors = &restore.mirrors;
            tracing::info!(
                display_id = ours,
                ?topology,
                ?mirrors,
                "display arrangement applied"
            );
            Some(restore)
        }
        Err(e) => {
            let error = format!("{e:#}");
            tracing::warn!(
                display_id = ours,
                ?topology,
                %error,
                "display arrangement not applied: extending"
            );
            None
        }
    }
}

/// Origins that put `ours` at (0, 0), the main display's place, every display keeping its
/// offset to the others. `None` when `ours` is not among them.
fn primary_origins(origins: &[(u32, i32, i32)], ours: u32) -> Option<Vec<(u32, i32, i32)>> {
    let &(_, ox, oy) = origins.iter().find(|o| o.0 == ours)?;
    Some(
        origins
            .iter()
            .map(|&(id, x, y)| (id, x - ox, y - oy))
            .collect(),
    )
}

/// The heads `exclusive` makes mirrors of `ours`: what it would darken elsewhere.
fn exclusive_mirrors(heads: &[PhysicalMonitor], ours: u32, keep: &[String]) -> Vec<u32> {
    crate::monitors::heads_to_darken(heads, &ours.to_string(), keep)
        .iter()
        .filter_map(|c| c.parse().ok())
        .collect()
}

/// One display configuration for this process: `apply` fills it, and it completes or cancels.
fn configure(apply: impl FnOnce(CGDisplayConfigRef) -> Result<()>) -> Result<()> {
    let mut cfg: CGDisplayConfigRef = std::ptr::null_mut();
    // SAFETY: `cfg` is a live local the call writes once.
    check(unsafe { CGBeginDisplayConfiguration(&mut cfg) })?;
    if let Err(e) = apply(cfg) {
        // SAFETY: `cfg` is the configuration begun above, not yet completed.
        unsafe { CGCancelDisplayConfiguration(cfg) };
        return Err(e);
    }
    // SAFETY: as above; completing it releases it.
    check(unsafe { CGCompleteDisplayConfiguration(cfg, CGConfigureOption::ForAppOnly) })
}

fn check(err: CGError) -> Result<()> {
    if err == CGError::Success {
        Ok(())
    } else {
        bail!("configure displays: CGError {}", err.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_head_keeps_points_for_its_origin_and_pixels_for_its_size() {
        let retina = head(1, (-1512.0, 0.0), (3024, 1964), 1512, 120.0, true, true);
        assert_eq!(retina.connector, "1");
        assert_eq!((retina.x, retina.y), (-1512, 0));
        assert_eq!((retina.width, retina.height), (3024, 1964));
        assert_eq!(retina.scale, 2.0);
        assert_eq!(retina.mode_label(), "3024x1964@120");
        assert!(retina.primary && retina.enabled && !retina.managed);

        let ntsc = head(
            722475312,
            (0.0, 0.0),
            (1920, 1080),
            1920,
            59.94,
            false,
            true,
        );
        assert_eq!(ntsc.refresh_mhz, 59_940);
        assert_eq!(ntsc.scale, 1.0);

        let modeless = head(3, (0.0, 0.0), (0, 0), 0, 0.0, false, false);
        assert_eq!((modeless.scale, modeless.refresh_mhz), (1.0, 0));
    }

    /// `primary` moves the virtual display to the origin and every other display by the same
    /// offset, so the arrangement keeps its shape.
    #[test]
    fn primary_moves_ours_to_the_origin_and_keeps_the_shape() {
        let before = [(3, 0, 0), (9, 5120, 0), (4, -1512, 200)];
        assert_eq!(
            primary_origins(&before, 9),
            Some(vec![(3, -5120, 0), (9, 0, 0), (4, -6632, 200)])
        );
        assert_eq!(primary_origins(&before, 7), None);
    }

    /// A display's serial is the same for the same device and mode, differs by mode, and steps
    /// past one a live display holds.
    #[test]
    fn a_serial_names_the_device_and_the_mode() {
        let mode = |w, h, hz: u32| Mode {
            width: w,
            height: h,
            refresh_hz: hz,
            refresh_mhz: hz * 1000,
        };
        let hd = serial(Some(1), mode(1920, 1080, 60), &[]);
        assert_eq!(hd, serial(Some(1), mode(1920, 1080, 60), &[]));
        assert_ne!(hd, serial(Some(1), mode(2560, 1440, 60), &[]));
        assert_ne!(hd, serial(Some(2), mode(1920, 1080, 60), &[]));
        assert_ne!(hd, serial(None, mode(1920, 1080, 60), &[]));
        assert_eq!(
            serial(Some(1), mode(1920, 1080, 60), &[hd]),
            hd.wrapping_add(1).max(1)
        );
        assert_ne!(hd, 0);
    }

    /// `exclusive` mirrors every lit head but ours, a kept one and one already dark.
    #[test]
    fn exclusive_mirrors_the_lit_heads_it_does_not_keep() {
        let mut heads = vec![
            head(3, (0.0, 0.0), (5120, 1440), 5120, 240.0, true, true),
            head(4, (-1512.0, 0.0), (3024, 1964), 1512, 120.0, false, true),
            head(5, (5120.0, 0.0), (1920, 1080), 1920, 60.0, false, false),
            head(9, (8000.0, 0.0), (1920, 1080), 1920, 60.0, false, true),
        ];
        heads[3].managed = true;
        assert_eq!(exclusive_mirrors(&heads, 9, &[]), [3, 4]);
        assert_eq!(exclusive_mirrors(&heads, 9, &["4".into()]), [3]);
    }

    /// Reads this Mac's displays: no permission, no change to any of them.
    #[test]
    #[ignore = "needs a Mac with a display"]
    fn the_main_display_is_listed_once_and_primary() {
        let heads = list_monitors().expect("list displays");
        let main = main_display();
        let mains: Vec<_> = heads.iter().filter(|h| h.connector == main).collect();
        assert_eq!(mains.len(), 1, "{heads:?}");
        assert!(mains[0].primary);
    }

    /// Makes a 1280x720 virtual display beside the others, keeps it for the same mode, makes it
    /// again for the larger 1920x1080, changes that to 1600x900 in place, has a join share it,
    /// and checks each is gone once its last output drops. Changes the desktop for a few seconds.
    #[test]
    #[ignore = "adds a display to this Mac's desktop"]
    fn a_virtual_display_lives_exactly_as_long_as_its_output() {
        if !creates_displays() {
            return;
        }
        let mut screen = OwnScreen {
            topology: Some(Topology::Extend),
            ..OwnScreen::default()
        };
        let mode = |w, h| Mode {
            width: w,
            height: h,
            refresh_hz: 60,
            refresh_mhz: 60_000,
        };
        let first = screen.create(mode(1280, 720)).expect("create");
        let id = first.cg_display.expect("display id");
        assert_eq!(first.preferred_mode.map(|m| (m.0, m.1)), Some((1280, 720)));
        let again = screen.create(mode(1280, 720)).expect("same mode");
        assert_eq!(
            again.cg_display,
            Some(id),
            "the same mode keeps the display"
        );
        let bigger = screen.create(mode(1920, 1080)).expect("new mode");
        let id2 = bigger.cg_display.expect("display id");
        assert!(id2 != id && !listed(id), "a new mode replaces the display");
        assert_eq!(
            bigger.preferred_mode.map(|m| (m.0, m.1)),
            Some((1920, 1080))
        );
        let smaller = screen.create(mode(1600, 900)).expect("smaller mode");
        assert_eq!(
            smaller.cg_display,
            Some(id2),
            "a smaller mode keeps the display"
        );
        assert_eq!(
            smaller.preferred_mode.map(|m| (m.0, m.1)),
            Some((1600, 900))
        );
        let mut joiner = OwnScreen {
            join: true,
            ..OwnScreen::default()
        };
        let joined = joiner.create(mode(1280, 720)).expect("join");
        assert_eq!(
            joined.cg_display,
            Some(id2),
            "a join shares the live display"
        );
        drop((first, again, bigger, smaller));
        assert!(listed(id2), "the joiner still holds it");
        drop(joined);
        assert!(!listed(id2));
        assert!(live().is_empty());
    }
}
