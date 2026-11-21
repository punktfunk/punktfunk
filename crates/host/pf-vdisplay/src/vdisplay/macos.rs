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
    CGDisplayIsActive, CGDisplayMode, CGDisplayModelNumber, CGDisplayVendorNumber, CGError,
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
/// The smallest size a virtual display allows in place: a mode switch inside it is a new mode
/// on the same display.
const MIN_MAX_PIXELS: (u32, u32) = (3840, 2160);
/// How long macOS gets to bring a display online, move it to a mode, or take it away.
const SETTLE: Duration = Duration::from_secs(3);

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
    let mode = mode.as_deref();
    let mut head = head(
        id,
        (origin.x, origin.y),
        (
            CGDisplayMode::pixel_width(mode),
            CGDisplayMode::pixel_height(mode),
        ),
        CGDisplayMode::width(mode),
        CGDisplayMode::refresh_rate(mode),
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

/// A session's own screen: a virtual display at the asked mode. A mode switch while the last
/// display it made lives sets the new mode on that display; a `join` session shares the oldest
/// live one. A display that can't be made falls back to a mirror of the main display, framed at
/// the asked size.
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
        if let Some(out) = self.resize(mode) {
            return Ok(out);
        }
        if self.join
            && let Some((screen, id)) =
                (live().into_iter()).find_map(|l| Some((l.screen.upgrade()?, l.id)))
        {
            tracing::info!(display_id = id, "sharing the live virtual display");
            return Ok(output(screen, id));
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
        let display = Display::create(&Spec {
            name: "Punktfunk",
            width,
            height,
            max: (width.max(MIN_MAX_PIXELS.0), height.max(MIN_MAX_PIXELS.1)),
            refresh_hz: f64::from(mode.millihz()) / 1000.0,
            vendor: VENDOR,
            product: PRODUCT,
            serial: self.slot.unwrap_or_else(anonymous_serial),
        })?;
        let id = display.id();
        settle("come online", || {
            let head = read(id, 0);
            head.enabled && (head.width, head.height) == (width, height)
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
        let screen = Screen::new(display, restore, mode, self.slot);
        let label = format!("{width}x{height}@{}", mode.refresh_hz);
        tracing::info!(display_id = id, mode = %label, ?topology, "virtual display created");
        crate::emit_display_event(crate::DisplayEvent::Created {
            backend: self.name().into(),
            width,
            height,
            refresh_hz: mode.refresh_hz,
        });
        self.live = Arc::downgrade(&screen);
        Ok(output(screen, id))
    }

    /// The new mode on the display this backend made, while a session still holds it.
    fn resize(&mut self, mode: Mode) -> Option<VirtualOutput> {
        let screen = self.live.upgrade()?;
        let (width, height) = (mode.width, mode.height);
        let id = {
            let held = screen.display.lock().unwrap_or_else(|e| e.into_inner());
            let held = held.as_ref()?;
            let id = held.id();
            if !held.fits(width, height) {
                tracing::info!(
                    display_id = id,
                    "the new mode outgrows the virtual display: making another"
                );
                return None;
            }
            let hz = f64::from(mode.millihz()) / 1000.0;
            if !matches!(held.apply(width, height, hz), Ok(true)) {
                tracing::warn!(
                    display_id = id,
                    "the virtual display refused the new mode: making another"
                );
                return None;
            }
            id
        };
        let moved = settle("take the new mode", || {
            let head = read(id, 0);
            (head.width, head.height) == (width, height)
        });
        if let Err(e) = moved {
            let error = format!("{e:#}");
            tracing::warn!(display_id = id, %error, "making another virtual display");
            return None;
        }
        set_live_mode(screen.entry, mode);
        let label = format!("{width}x{height}@{}", mode.refresh_hz);
        tracing::info!(
            display_id = id,
            mode = %label,
            "virtual display moved to a new mode in place"
        );
        Some(output(screen, id))
    }
}

fn output(screen: Arc<Screen>, id: u32) -> VirtualOutput {
    let head = read(id, 0);
    let refresh_hz = head.refresh_mhz.saturating_add(500) / 1000;
    let mut out = VirtualOutput::owned(
        0,
        Some((head.width, head.height, refresh_hz)),
        Box::new(screen),
    );
    out.refresh_mhz = Some(head.refresh_mhz).filter(|&mhz| mhz > 0);
    out.cg_display = Some(id);
    out
}

/// A serial for a session with no identity slot: never one a slot takes (1..=15).
fn anonymous_serial() -> u32 {
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0x100);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Poll `done` until it holds, for at most [`SETTLE`].
fn settle(what: &str, done: impl Fn() -> bool) -> Result<()> {
    let start = Instant::now();
    while !done() {
        if start.elapsed() >= SETTLE {
            bail!("the display did not {what} within {SETTLE:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Ok(())
}

/// One live virtual display, held by every output that streams it.
struct Screen {
    display: Mutex<Option<Display>>,
    restore: Option<Restore>,
    entry: u64,
}

impl Screen {
    /// The display, listed live until its last holder drops it.
    fn new(
        display: Display,
        restore: Option<Restore>,
        mode: Mode,
        slot: Option<u32>,
    ) -> Arc<Screen> {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let entry = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let id = display.id();
        let screen = Arc::new(Screen {
            display: Mutex::new(Some(display)),
            restore,
            entry,
        });
        lock_live().push(Live {
            entry,
            id,
            mode: (mode.width, mode.height, mode.refresh_hz),
            slot,
            screen: Arc::downgrade(&screen),
        });
        screen
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        if let Some(restore) = self.restore.take() {
            restore.run();
        }
        let display = self
            .display
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        let id = display.as_ref().map(Display::id);
        drop(display);
        if let Some(id) = id {
            match settle("go away", || !online().is_ok_and(|ids| ids.contains(&id))) {
                Ok(()) => tracing::info!(display_id = id, "virtual display removed"),
                Err(e) => {
                    let error = format!("{e:#}");
                    tracing::warn!(
                        display_id = id,
                        %error,
                        "virtual display still listed after release"
                    );
                }
            }
        }
        lock_live().retain(|l| l.entry != self.entry);
        crate::emit_display_event(crate::DisplayEvent::Released { count: 1 });
    }
}

/// A live virtual display, as `/display/state` lists it.
#[derive(Clone)]
struct Live {
    entry: u64,
    id: u32,
    mode: (u32, u32, u32),
    slot: Option<u32>,
    screen: Weak<Screen>,
}

static LIVE: Mutex<Vec<Live>> = Mutex::new(Vec::new());

fn lock_live() -> std::sync::MutexGuard<'static, Vec<Live>> {
    LIVE.lock().unwrap_or_else(|e| e.into_inner())
}

fn live() -> Vec<Live> {
    lock_live().clone()
}

fn set_live_mode(entry: u64, mode: Mode) {
    if let Some(l) = lock_live().iter_mut().find(|l| l.entry == entry) {
        l.mode = (mode.width, mode.height, mode.refresh_hz);
    }
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
    /// Unmirror the heads and put every display still listed back where it was.
    fn run(self) {
        let listed = online().unwrap_or_default();
        let result = configure(|cfg| {
            for &head in self.mirrors.iter().filter(|h| listed.contains(h)) {
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

    /// Makes a 1280x720 virtual display beside the others, moves it to 1920x1080 in place, has a
    /// join share it, and checks it is gone once the last output drops. Changes the desktop for
    /// a few seconds.
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
        assert!(online().unwrap().contains(&id));
        let second = screen.create(mode(1920, 1080)).expect("resize");
        assert_eq!(second.cg_display, Some(id), "resized in place");
        assert_eq!(
            second.preferred_mode.map(|m| (m.0, m.1)),
            Some((1920, 1080))
        );
        let mut joiner = OwnScreen {
            join: true,
            ..OwnScreen::default()
        };
        let joined = joiner.create(mode(1280, 720)).expect("join");
        assert_eq!(
            joined.cg_display,
            Some(id),
            "a join shares the live display"
        );
        drop((first, second));
        assert!(online().unwrap().contains(&id), "the joiner still holds it");
        drop(joined);
        assert!(!online().unwrap().contains(&id));
        assert!(live().iter().all(|l| l.id != id));
    }
}
