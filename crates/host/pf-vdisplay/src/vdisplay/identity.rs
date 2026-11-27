//! Per-client → stable display-id map. A reconnect reuses the same small id so
//! the compositor can reapply per-display scale. Design: `design/display-management.md`.
//!
//! Ids stay `1..=15` on every platform (Windows IddCx `ConnectorIndex` is
//! `< MaxMonitorsSupported` = 16). The key is cert fingerprint, or fingerprint
//! plus resolution (`per-client-mode`). Sessions with no fingerprint never
//! reach this map (id `0` / auto, upstream).
//!
//! Persist path: `<config>/display-identity.json` (migrates
//! `pf-vdisplay-identity.json`). GNOME cannot rematch a virtual monitor and Windows
//! rematches only an unchanged EDID, so [`ScaleMap`] stores scale under the same
//! [`identity_key`].

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};

/// IddCx `ConnectorIndex` is `< MaxMonitorsSupported` (16). Shared map: `1..=15`.
const MAX_ID: u32 = 15;
const RESERVED_CONSOLE_MAX_ID: u32 = 11;
const FIRST_SEAT_ID: u32 = 12;
const SEAT_SLOT_ENV: &str = "PUNKTFUNK_SEAT_DISPLAY_SLOT";
const SEATS_REGISTRY_KEY: &str = r"HKLM\SOFTWARE\Punktfunk\Seats";

/// Process role derived from the trusted seats reservation and the optional
/// fixed connector assignment. The console role retains slot 0 for clients
/// without an identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WindowsSlotPlan {
    Unreserved,
    ReservedConsole,
    Seat(u32),
}

impl WindowsSlotPlan {
    pub(crate) const fn ordinary_max(self) -> Option<u32> {
        match self {
            Self::Unreserved => Some(MAX_ID),
            Self::ReservedConsole => Some(RESERVED_CONSOLE_MAX_ID),
            Self::Seat(_) => None,
        }
    }

    pub(crate) const fn seat_slot(self) -> Option<u32> {
        match self {
            Self::Seat(slot) => Some(slot),
            Self::Unreserved | Self::ReservedConsole => None,
        }
    }

    /// The `resolved` slot as a joiner's own, which a join skips. `None` when other clients
    /// share it: a seat's one slot, and slot 0 for every client without an identity slot.
    pub(crate) fn own_slot(self, resolved: u32) -> Option<u32> {
        (self.seat_slot().is_none() && resolved != 0).then_some(resolved)
    }

    /// The connector a console client drives once it resolved to `resolved`. Slot 0 takes one
    /// device at a time: a device on a live connector keeps it, and while another holds slot 0
    /// this one takes `device_slot`, its own. `held` is each live slot and its device.
    pub(crate) fn connector(
        self,
        resolved: u32,
        fp: Option<[u8; 32]>,
        held: &[(u32, Option<[u8; 32]>)],
        device_slot: impl FnOnce() -> Option<u32>,
    ) -> u32 {
        if resolved != 0 || fp.is_none() || self.seat_slot().is_some() {
            return resolved;
        }
        if let Some(&(slot, _)) = held.iter().find(|(_, device)| *device == fp) {
            return slot;
        }
        if held.iter().any(|&(slot, _)| slot == 0) {
            return device_slot().unwrap_or(0);
        }
        0
    }

    /// Applies the role to a normal host's resolved slot. Seat assignment
    /// replaces that slot; console roles validate their allowed connector set.
    pub(crate) fn resolve(self, ordinary_slot: u32) -> Result<u32, WindowsSlotError> {
        let Some(max) = self.ordinary_max() else {
            return Ok(self.seat_slot().expect("seat plan has a slot"));
        };
        if ordinary_slot <= max {
            Ok(ordinary_slot)
        } else {
            Err(WindowsSlotError::OrdinaryOutOfRange {
                slot: ordinary_slot,
                max,
            })
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WindowsSlotError {
    MalformedOverride,
    OverrideOutOfRange(u32),
    ReservationMissing,
    OrdinaryOutOfRange { slot: u32, max: u32 },
}

impl std::fmt::Display for WindowsSlotError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MalformedOverride => write!(
                f,
                "{SEAT_SLOT_ENV} must be a decimal connector slot in {FIRST_SEAT_ID}..={MAX_ID}"
            ),
            Self::OverrideOutOfRange(slot) => write!(
                f,
                "{SEAT_SLOT_ENV}={slot} is outside the reserved connector slots {FIRST_SEAT_ID}..={MAX_ID}"
            ),
            Self::ReservationMissing => write!(
                f,
                "{SEAT_SLOT_ENV} requires the seats reservation marker at {SEATS_REGISTRY_KEY}"
            ),
            Self::OrdinaryOutOfRange { slot, max } => write!(
                f,
                "ordinary pf-vdisplay connector slot {slot} is outside the active 0..={max} range"
            ),
        }
    }
}

impl std::error::Error for WindowsSlotError {}

/// Resolves the process role without reading environment or registry state.
/// A seat override is valid only inside the reserved range and while the
/// machine-level reservation is active.
pub(crate) fn windows_slot_plan(
    seat_override: Option<&OsStr>,
    reservation_enabled: bool,
) -> Result<WindowsSlotPlan, WindowsSlotError> {
    let Some(raw) = seat_override else {
        return Ok(if reservation_enabled {
            WindowsSlotPlan::ReservedConsole
        } else {
            WindowsSlotPlan::Unreserved
        });
    };
    let slot = raw
        .to_str()
        .ok_or(WindowsSlotError::MalformedOverride)?
        .parse::<u32>()
        .map_err(|_| WindowsSlotError::MalformedOverride)?;
    if !(FIRST_SEAT_ID..=MAX_ID).contains(&slot) {
        return Err(WindowsSlotError::OverrideOutOfRange(slot));
    }
    if !reservation_enabled {
        return Err(WindowsSlotError::ReservationMissing);
    }
    Ok(WindowsSlotPlan::Seat(slot))
}

const FILE: &str = "display-identity.json";
const LEGACY_FILE: &str = "pf-vdisplay-identity.json";

/// Fingerprint hex; `{hex}@{w}x{h}` when `per_client_mode` so each resolution keeps its scale.
pub(crate) fn identity_key(fp: [u8; 32], mode: (u32, u32), per_client_mode: bool) -> String {
    let hex = hex::encode(fp);
    if per_client_mode {
        format!("{hex}@{}x{}", mode.0, mode.1)
    } else {
        hex
    }
}

#[derive(Serialize, Deserialize, Default)]
struct Store {
    /// MRU counter; persisted so LRU order survives a host restart.
    tick: u64,
    entries: Vec<Entry>,
}

#[derive(Serialize, Deserialize)]
struct Entry {
    /// Serialized as `fp` for back-compat with `pf-vdisplay-identity.json`.
    #[serde(rename = "fp")]
    key: String,
    id: u32,
    seen: u64,
}

pub(crate) struct DisplayIdentityMap {
    path: PathBuf,
    store: Store,
    /// False when the map exists but could not be read: this run remembers nothing and
    /// writes nothing, so the file is still there for the next start.
    persists: bool,
}

impl DisplayIdentityMap {
    pub(crate) fn load() -> Self {
        Self::load_in(&pf_paths::config_dir())
    }

    /// Empty on first run or parse failure (ids re-derive once). A missing map falls back to
    /// `pf-vdisplay-identity.json`; a map the OS refused is left alone (`persists` false).
    fn load_in(dir: &std::path::Path) -> Self {
        let path = dir.join(FILE);
        let (from, bytes, persists) = match std::fs::read(&path) {
            Ok(b) => (path.clone(), Some(b), true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let legacy = dir.join(LEGACY_FILE);
                match std::fs::read(&legacy) {
                    Ok(b) => (legacy, Some(b), true),
                    Err(_) => (path.clone(), None, true),
                }
            }
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e,
                    "display-identity map exists but could not be read — ids are not remembered \
                     this run and the file is left as it is");
                (path.clone(), None, false)
            }
        };
        let mut store = match bytes {
            Some(b) => match serde_json::from_slice::<Store>(&b) {
                Ok(s) => s,
                Err(e) => {
                    // Rename aside so the next persist cannot overwrite an unreadable map
                    // (same as `display-presets.json`). Recover by hand from `.json.bad`.
                    tracing::warn!(
                        path = %from.display(),
                        error = %e,
                        "display-identity map is unreadable — starting a fresh one; \
                         the old file is kept as .bad (every client re-derives its display id once)"
                    );
                    let _ = std::fs::rename(&from, from.with_extension("json.bad"));
                    Store::default()
                }
            },
            None => Store::default(),
        };
        // `resolve` returns a stored id as-is. Drop 0 / >MAX_ID and duplicate key or id
        // (keep MRU) so a hand-edited file cannot collide two clients onto one slot.
        store.entries.sort_by_key(|e| std::cmp::Reverse(e.seen));
        let mut seen_key = std::collections::HashSet::new();
        let mut seen_id = std::collections::HashSet::new();
        store.entries.retain(|e| {
            (1..=MAX_ID).contains(&e.id) && seen_key.insert(e.key.clone()) && seen_id.insert(e.id)
        });
        Self {
            path,
            store,
            persists,
        }
    }

    /// Returns the remembered id, or the lowest free / LRU-idle id in
    /// `1..=max_id`. Live ids are never evicted because the Windows slot map
    /// joins a newcomer to the monitor already there. A remembered id above
    /// the active bound migrates on its next resolution.
    pub(crate) fn resolve_bounded(
        &mut self,
        key: &str,
        live: &BTreeSet<u32>,
        max_id: u32,
    ) -> Option<u32> {
        debug_assert!((1..=MAX_ID).contains(&max_id));
        self.store.tick = self.store.tick.wrapping_add(1);
        let now = self.store.tick;

        if let Some(e) = self
            .store
            .entries
            .iter_mut()
            .find(|e| e.key == key && e.id <= max_id)
        {
            e.seen = now;
            let id = e.id;
            self.persist();
            return Some(id);
        }

        let id = match (1..=max_id).find(|i| !self.store.entries.iter().any(|e| e.id == *i)) {
            Some(free) => free,
            None => {
                let lru = self
                    .store
                    .entries
                    .iter()
                    .filter(|e| e.id <= max_id && !live.contains(&e.id))
                    .min_by_key(|e| e.seen)
                    .map(|e| e.id);
                let Some(lru) = lru else {
                    tracing::warn!(
                        cap = max_id,
                        live = live.len(),
                        "display identity map is full and every id is driving a live display — \
                         this client gets the shared/auto display identity (no persisted per-client \
                         scaling) rather than displacing a live one"
                    );
                    return None;
                };
                lru
            }
        };
        self.store.entries.retain(|e| e.key != key && e.id != id);
        self.store.entries.push(Entry {
            key: key.to_string(),
            id,
            seen: now,
        });
        self.persist();
        Some(id)
    }

    /// Resolves against the full identity range used outside a seats reservation.
    pub(crate) fn resolve(&mut self, key: &str, live: &BTreeSet<u32>) -> Option<u32> {
        self.resolve_bounded(key, live, MAX_ID)
    }

    /// Best-effort ([`write_private`]); nothing while `persists` is false.
    fn persist(&self) {
        if !self.persists {
            return;
        }
        let Ok(bytes) = serde_json::to_vec_pretty(&self.store) else {
            return;
        };
        write_private(&self.path, &bytes, "the display-identity map");
    }
}

/// Write one of this module's files under `config_dir()` (the identity and scale maps): the
/// parent is `create_private_dir` (0700 on Unix), never `create_dir_all`. A write that fails is
/// logged, not dropped: the next start re-derives what it should have found.
fn write_private(path: &std::path::Path, bytes: &[u8], what: &str) {
    if let Some(dir) = path.parent() {
        let _ = pf_paths::create_private_dir(dir);
    }
    if let Err(e) = pf_paths::replace_file(path, bytes) {
        tracing::warn!(path = %path.display(), error = %e, "{what} was not written");
    }
}

/// Process-wide map, loaded once. Seat processes bypass it; the one console
/// owner is the only process that writes `display-identity.json`.
pub(crate) fn global() -> &'static Mutex<DisplayIdentityMap> {
    static MAP: OnceLock<Mutex<DisplayIdentityMap>> = OnceLock::new();
    MAP.get_or_init(|| Mutex::new(DisplayIdentityMap::load()))
}

/// Resolves a client under the configured identity policy. `None` means
/// shared/anonymous, or that every candidate id is live. `max_id` bounds the
/// identity map so a Windows console host cannot enter reserved seat slots.
pub(crate) fn resolve_slot_bounded(
    fp: Option<[u8; 32]>,
    mode: (u32, u32),
    default: crate::policy::Identity,
    max_id: u32,
) -> Option<u32> {
    use crate::policy::Identity;
    // Per device: "remember display settings" is a property of the device whose
    // settings are being remembered (§6.1).
    let id_policy = identity_policy_for(fp, default);
    let per_client_mode = match id_policy {
        Identity::Shared => return None,
        Identity::PerClient => false,
        Identity::PerClientMode => true,
    };
    device_slot_bounded(fp?, mode, per_client_mode, max_id)
}

/// `fp`'s own slot in the identity map, whatever its policy says. `None` when every
/// candidate id is live.
pub(crate) fn device_slot_bounded(
    fp: [u8; 32],
    mode: (u32, u32),
    per_client_mode: bool,
    max_id: u32,
) -> Option<u32> {
    // Sample live ids before the map lock. Their sources take the manager/pool
    // lock, and this map is reached from backend `create`. Map lock stays a leaf.
    let live = live_slot_ids();
    let slot = global().lock().unwrap().resolve_bounded(
        &identity_key(fp, mode, per_client_mode),
        &live,
        max_id,
    );
    if let Some(slot) = slot {
        remember_slot_owner(slot, fp);
    }
    slot
}

/// Which device owns each identity slot, as hex.
///
/// The display registry knows a display's slot but never its client: `acquire` is reached
/// from a backend `create` with no session in scope, and teardown runs on the linger thread
/// long after. This map is the one place both are known at once, so it is what lets a kept
/// display resolve its owner's per-device policy (`design/web-console-overhaul.md` §6.1).
///
/// Bounded by the slot table (1..=MAX_ID), so it cannot grow. An entry is overwritten when
/// the slot is reassigned, which is exactly when the owner changed.
static SLOT_OWNER: Mutex<BTreeMap<u32, String>> = Mutex::new(BTreeMap::new());

fn remember_slot_owner(slot: u32, fp: [u8; 32]) {
    let hex = hex::encode(fp);
    SLOT_OWNER
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(slot, hex);
}

/// The device that owns this identity slot, if one has been resolved for it.
///
/// `None` means shared or anonymous — that device follows the host policy, which is the
/// right answer rather than a missing one.
pub(crate) fn slot_owner(slot: Option<u32>) -> Option<String> {
    let slot = slot?;
    SLOT_OWNER
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&slot)
        .cloned()
}

/// Whether device `fp` may take a kept display carrying `slot`. A display with a slot goes back
/// to the device that owns it; one with none (shared, anonymous) to whoever asks first. Every
/// pooled backend's `accepts_kept` answers through here. Takes only the leaf owner lock.
pub(crate) fn may_reuse(slot: Option<u32>, fp: Option<[u8; 32]>) -> bool {
    slot.is_none() || slot_owner(slot) == crate::policy::fp_hex(fp)
}

/// The slot [`resolve_slot_bounded`] answers for `fp` now, without allocating one or touching
/// the LRU order. `None` for a shared or anonymous client, or one with no slot yet.
pub(crate) fn peek_slot(
    fp: Option<[u8; 32]>,
    mode: (u32, u32),
    default: crate::policy::Identity,
) -> Option<u32> {
    use crate::policy::Identity;
    let per_client_mode = match identity_policy_for(fp, default) {
        Identity::Shared => return None,
        Identity::PerClient => false,
        Identity::PerClientMode => true,
    };
    let key = identity_key(fp?, mode, per_client_mode);
    let map = global().lock().unwrap();
    map.store
        .entries
        .iter()
        .find(|e| e.key == key)
        .map(|e| e.id)
}

/// Resolves against the full identity range used by non-Windows backends.
pub(crate) fn resolve_slot(
    fp: Option<[u8; 32]>,
    mode: (u32, u32),
    default: crate::policy::Identity,
) -> Option<u32> {
    resolve_slot_bounded(fp, mode, default, MAX_ID)
}

/// Ids driving a real display, including KEPT (lingering) ones: the reconnect
/// must find that slot again. `0` is not an identity and does not block assignment.
fn live_slot_ids() -> BTreeSet<u32> {
    #[cfg(target_os = "windows")]
    {
        crate::manager::snapshot()
            .into_iter()
            .map(|i| i.slot_id)
            .filter(|s| *s != 0)
            .collect()
    }
    #[cfg(target_os = "linux")]
    {
        crate::registry::live_identity_slots()
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    {
        BTreeSet::new()
    }
}

const SCALE_FILE: &str = "display-scale.json";

/// Scale-map key: [`identity_key`], or `"shared"` for Shared/anonymous, under the device's
/// own identity policy. `"shared"` cannot collide — identity keys are 64 hex chars.
pub(crate) fn scale_key(
    fp: Option<[u8; 32]>,
    mode: (u32, u32),
    default: crate::policy::Identity,
) -> String {
    scale_key_for(identity_policy_for(fp, default), fp, mode)
}

/// The identity axis for `fp`: the device's own overlay, else the host's, else `default`.
/// The slot and the scale key both resolve through here, so they cannot disagree.
fn identity_policy_for(
    fp: Option<[u8; 32]>,
    default: crate::policy::Identity,
) -> crate::policy::Identity {
    crate::policy::prefs()
        .configured()
        .map(|p| {
            p.effective_for(crate::policy::fp_hex(fp).as_deref())
                .identity
        })
        .unwrap_or(default)
}

/// [`scale_key`] with policy already resolved (no global prefs).
fn scale_key_for(
    policy: crate::policy::Identity,
    fp: Option<[u8; 32]>,
    mode: (u32, u32),
) -> String {
    use crate::policy::Identity;
    match (policy, fp) {
        (Identity::Shared, _) | (_, None) => "shared".to_string(),
        (Identity::PerClient, Some(fp)) => identity_key(fp, mode, false),
        (Identity::PerClientMode, Some(fp)) => identity_key(fp, mode, true),
    }
}

/// The scale-map key of a Windows host: the device and the size of its display. Windows keeps
/// a scale per display size already, and the host only stops it being forgotten. `None` for
/// a device with no identity of its own, whose display other devices use too.
pub(crate) fn sized_scale_key(
    fp: Option<[u8; 32]>,
    mode: (u32, u32),
    default: crate::policy::Identity,
) -> Option<String> {
    sized_scale_key_for(identity_policy_for(fp, default), fp, mode)
}

/// [`sized_scale_key`] with policy already resolved (no global prefs).
fn sized_scale_key_for(
    policy: crate::policy::Identity,
    fp: Option<[u8; 32]>,
    mode: (u32, u32),
) -> Option<String> {
    if policy == crate::policy::Identity::Shared {
        return None;
    }
    Some(identity_key(fp?, mode, true))
}

/// Client-key → desktop-scale, for the desktops that forget it. GNOME never rematches
/// `RecordVirtual` EDIDs (fresh serial, no override). Windows files a scale under the exact
/// EDID, which changes with the mode, the refresh rate and the client's panel. On both the
/// host stores the scale and its backend reapplies it. KDE keeps the scale once the id is
/// stable, the host's own included: the `kwin:<output>` and `kwin` keys are the scales the
/// host set last, on that output and on any, which tell them from one the player set.
pub(crate) struct ScaleMap {
    path: PathBuf,
    map: std::collections::BTreeMap<String, f64>,
}

impl ScaleMap {
    fn load() -> Self {
        Self::load_from(pf_paths::config_dir().join(SCALE_FILE))
    }

    /// Empty on first run. One unreadable value loses only its entry; a non-finite scale or
    /// one outside 0.25..=8.0 (a hand-edited file can store anything) is dropped too. A file
    /// that is not a map, or that the OS refused, keeps nothing and says so.
    fn load_from(path: PathBuf) -> Self {
        let raw: std::collections::BTreeMap<String, serde_json::Value> = match std::fs::read(&path)
        {
            Ok(b) => serde_json::from_slice(&b).unwrap_or_else(|e| {
                tracing::warn!(path = %path.display(), error = %e,
                        "display-scale.json is not a map — every remembered scale is forgotten");
                Default::default()
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Default::default(),
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e,
                        "display-scale.json could not be read — remembered scales do not apply");
                Default::default()
            }
        };
        let map = raw
            .into_iter()
            .filter_map(|(k, v)| {
                let s = v.as_f64()?;
                (s.is_finite() && (0.25..=8.0).contains(&s)).then_some((k, s))
            })
            .collect();
        Self { path, map }
    }

    pub(crate) fn get(&self, key: &str) -> Option<f64> {
        self.map.get(key).copied()
    }

    pub(crate) fn set(&mut self, key: &str, scale: f64) {
        if !scale.is_finite() || !(0.25..=8.0).contains(&scale) {
            return;
        }
        self.map.insert(key.to_string(), scale);
        let Ok(bytes) = serde_json::to_vec_pretty(&self.map) else {
            return;
        };
        write_private(&self.path, &bytes, "the remembered scale");
    }
}

/// Process-wide scale map, loaded once. The Mutter and KWin backends and the Windows manager
/// use it.
pub(crate) fn scales() -> &'static Mutex<ScaleMap> {
    static MAP: OnceLock<Mutex<ScaleMap>> = OnceLock::new();
    MAP.get_or_init(|| Mutex::new(ScaleMap::load()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fp(n: u8) -> [u8; 32] {
        let mut f = [0u8; 32];
        f[0] = n;
        f
    }

    /// Its own dir: every persist re-ACLs the parent on Windows, and `%TEMP%` itself is huge.
    fn temp_map(tag: &str) -> DisplayIdentityMap {
        let dir = std::env::temp_dir().join(format!("pf-id-{tag}-{}", std::process::id()));
        DisplayIdentityMap {
            path: dir.join(FILE),
            store: Store::default(),
            persists: true,
        }
    }

    #[test]
    fn an_unreadable_identity_map_is_neither_replaced_nor_resurrected() {
        // A directory where the map should be: the read fails, but not with NotFound. The
        // stale legacy map beside it must not come back, and nothing may overwrite the file.
        let dir = std::env::temp_dir().join(format!("pf-id-unreadable-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(FILE)).unwrap();
        std::fs::write(
            dir.join(LEGACY_FILE),
            br#"{"tick":1,"entries":[{"fp":"old","id":3,"seen":1}]}"#,
        )
        .unwrap();
        let map = DisplayIdentityMap::load_in(&dir);
        assert!(
            map.store.entries.is_empty(),
            "the legacy map is not resurrected"
        );
        assert!(!map.persists, "nothing overwrites the file this run");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A peek answers what a resolve remembered and takes no slot itself.
    #[test]
    fn a_peek_reads_a_slot_without_allocating_one() {
        let name = "a_peek_reads_a_slot_without_allocating_one";
        crate::fake::in_child(module_path!(), name, &[], || {
            use crate::policy::Identity::{PerClient, Shared};
            assert_eq!(peek_slot(Some(fp(1)), (1920, 1080), PerClient), None);
            assert_eq!(resolve_slot(Some(fp(2)), (1920, 1080), PerClient), Some(1));
            assert_eq!(peek_slot(Some(fp(2)), (1280, 720), PerClient), Some(1));
            assert_eq!(peek_slot(Some(fp(2)), (1920, 1080), Shared), None);
            assert_eq!(peek_slot(None, (1920, 1080), PerClient), None);
        });
    }

    /// A slotted kept display is its owner's alone; a slotless one is anyone's.
    #[test]
    fn a_kept_display_is_its_owners_or_anyones() {
        let name = "a_kept_display_is_its_owners_or_anyones";
        crate::fake::in_child(module_path!(), name, &[], || {
            let slot = resolve_slot(
                Some(fp(1)),
                (1920, 1080),
                crate::policy::Identity::PerClient,
            );
            assert!(slot.is_some());
            let answers = [Some(fp(1)), Some(fp(2)), None].map(|fp| may_reuse(slot, fp));
            assert_eq!(answers, [true, false, false]);
            assert!(may_reuse(None, Some(fp(2))) && may_reuse(None, None));
        });
    }

    fn nothing_live() -> BTreeSet<u32> {
        BTreeSet::new()
    }

    #[test]
    fn windows_normal_roles_keep_anonymous_at_zero() {
        let unreserved = windows_slot_plan(None, false).unwrap();
        assert_eq!(unreserved, WindowsSlotPlan::Unreserved);
        assert_eq!(unreserved.ordinary_max(), Some(15));
        assert_eq!(unreserved.resolve(0), Ok(0));
        assert_eq!(unreserved.resolve(15), Ok(15));

        let reserved = windows_slot_plan(None, true).unwrap();
        assert_eq!(reserved, WindowsSlotPlan::ReservedConsole);
        assert_eq!(reserved.ordinary_max(), Some(11));
        assert_eq!(reserved.resolve(0), Ok(0));
        assert_eq!(reserved.resolve(11), Ok(11));
        assert!(matches!(
            reserved.resolve(12),
            Err(WindowsSlotError::OrdinaryOutOfRange { slot: 12, max: 11 })
        ));
    }

    #[test]
    fn windows_seat_override_accepts_only_reserved_slots() {
        for slot in 12..=15 {
            let raw = slot.to_string();
            let plan = windows_slot_plan(Some(OsStr::new(&raw)), true).unwrap();
            assert_eq!(plan, WindowsSlotPlan::Seat(slot));
            assert_eq!(plan.resolve(0), Ok(slot));
            assert_eq!(plan.resolve(7), Ok(slot));
        }
        for slot in [0, 1, 11, 16, u32::MAX] {
            let raw = slot.to_string();
            assert_eq!(
                windows_slot_plan(Some(OsStr::new(&raw)), true),
                Err(WindowsSlotError::OverrideOutOfRange(slot))
            );
        }
    }

    #[test]
    fn a_shared_slot_is_no_joiners_own() {
        for plan in [
            WindowsSlotPlan::Unreserved,
            WindowsSlotPlan::ReservedConsole,
        ] {
            assert_eq!(plan.own_slot(0), None);
            assert_eq!(plan.own_slot(3), Some(3));
        }
        assert_eq!(WindowsSlotPlan::Seat(12).own_slot(12), None);
    }

    #[test]
    fn slot_0_takes_one_device_at_a_time() {
        let console = WindowsSlotPlan::Unreserved;
        let (a, b) = (Some([1; 32]), Some([2; 32]));
        let own = || Some(4);
        let none = || None;
        assert_eq!(console.connector(0, a, &[], own), 0);
        // A device's rebuild stays where it is, before and after the other one leaves.
        assert_eq!(console.connector(0, a, &[(0, a), (4, b)], none), 0);
        assert_eq!(console.connector(0, b, &[(4, b)], none), 4);
        // Another device, or a Moonlight session, holds slot 0.
        assert_eq!(console.connector(0, b, &[(0, a)], own), 4);
        assert_eq!(console.connector(0, b, &[(0, None)], own), 4);
        // No free identity slot leaves it where it was.
        assert_eq!(console.connector(0, b, &[(0, a)], none), 0);
        // No fingerprint, a seat, or an identity slot of its own: untouched.
        assert_eq!(console.connector(0, None, &[(0, a)], own), 0);
        assert_eq!(
            WindowsSlotPlan::Seat(12).connector(12, b, &[(12, a)], own),
            12
        );
        assert_eq!(console.connector(3, b, &[(0, a)], own), 3);
    }

    #[test]
    fn windows_seat_override_fails_closed_without_reservation() {
        assert_eq!(
            windows_slot_plan(Some(OsStr::new("12")), false),
            Err(WindowsSlotError::ReservationMissing)
        );
        for raw in ["", "seat-12", " 12", "12 ", "-12", "4294967296"] {
            assert_eq!(
                windows_slot_plan(Some(OsStr::new(raw)), true),
                Err(WindowsSlotError::MalformedOverride),
                "{raw:?}"
            );
        }
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            assert_eq!(
                windows_slot_plan(Some(OsStr::from_bytes(b"\xff")), true),
                Err(WindowsSlotError::MalformedOverride)
            );
        }
    }

    #[test]
    fn bounded_identity_map_never_allocates_reserved_seat_slots() {
        let mut m = temp_map("seat-bound");
        for n in 1..=24u8 {
            let id = m
                .resolve_bounded(
                    &identity_key(fp(n), (1920, 1080), false),
                    &nothing_live(),
                    RESERVED_CONSOLE_MAX_ID,
                )
                .unwrap();
            assert!((1..=RESERVED_CONSOLE_MAX_ID).contains(&id), "fp {n}: {id}");
        }
        assert!(m
            .store
            .entries
            .iter()
            .all(|e| e.id <= RESERVED_CONSOLE_MAX_ID));
        let all_console_slots_live: BTreeSet<u32> = (1..=RESERVED_CONSOLE_MAX_ID).collect();
        assert_eq!(
            m.resolve_bounded(
                &identity_key(fp(25), (1920, 1080), false),
                &all_console_slots_live,
                RESERVED_CONSOLE_MAX_ID,
            ),
            None,
            "a full console range must not spill into a reserved seat slot"
        );
        let _ = std::fs::remove_dir_all(m.path.parent().unwrap());
    }

    #[test]
    fn bounded_identity_map_migrates_a_remembered_reserved_id() {
        let mut m = temp_map("seat-migrate");
        let mut key = String::new();
        for n in 1..=12u8 {
            key = identity_key(fp(n), (1920, 1080), false);
            assert_eq!(m.resolve(&key, &nothing_live()), Some(u32::from(n)));
        }
        let migrated = m
            .resolve_bounded(&key, &nothing_live(), RESERVED_CONSOLE_MAX_ID)
            .unwrap();
        assert!((1..=RESERVED_CONSOLE_MAX_ID).contains(&migrated));
        assert_eq!(
            m.store.entries.iter().find(|e| e.key == key).unwrap().id,
            migrated
        );
        let _ = std::fs::remove_dir_all(m.path.parent().unwrap());
    }

    #[test]
    fn stable_across_calls_and_distinct_per_client() {
        let mut m = temp_map("stable");
        let a1 = m.resolve(&identity_key(fp(1), (1920, 1080), false), &nothing_live());
        let b = m.resolve(&identity_key(fp(2), (1920, 1080), false), &nothing_live());
        let a2 = m.resolve(&identity_key(fp(1), (1280, 720), false), &nothing_live());
        assert_eq!(a1, a2, "same client → same id (per-client ignores mode)");
        assert_ne!(a1, b, "distinct clients → distinct ids");
        assert!(a1.is_some_and(|i| (1..=MAX_ID).contains(&i)));
        assert!(b.is_some_and(|i| (1..=MAX_ID).contains(&i)));
        let _ = std::fs::remove_dir_all(m.path.parent().unwrap());
    }

    #[test]
    fn per_client_mode_splits_by_resolution() {
        let mut m = temp_map("permode");
        let hd = m.resolve(&identity_key(fp(1), (1920, 1080), true), &nothing_live());
        let uhd = m.resolve(&identity_key(fp(1), (3840, 2160), true), &nothing_live());
        let hd2 = m.resolve(&identity_key(fp(1), (1920, 1080), true), &nothing_live());
        assert_ne!(hd, uhd, "same client, different resolution → different id");
        assert_eq!(hd, hd2, "same client + resolution → same id");
        let _ = std::fs::remove_dir_all(m.path.parent().unwrap());
    }

    #[test]
    fn lru_eviction_reuses_an_id_at_the_cap() {
        let mut m = temp_map("lru");
        for n in 1..=15u8 {
            m.resolve(&identity_key(fp(n), (1920, 1080), false), &nothing_live());
        }
        // Touch 2 so 1 is LRU.
        let _ = m.resolve(&identity_key(fp(2), (1920, 1080), false), &nothing_live());
        let id16 = m
            .resolve(&identity_key(fp(16), (1920, 1080), false), &nothing_live())
            .expect("nothing is live → the LRU id is free to take");
        assert!((1..=MAX_ID).contains(&id16));
        assert_eq!(m.store.entries.len(), 15, "cap holds at 15 entries");
        assert!(m.store.entries.iter().all(|e| (1..=MAX_ID).contains(&e.id)));
        let _ = std::fs::remove_dir_all(m.path.parent().unwrap());
    }

    #[test]
    fn lru_eviction_never_takes_a_live_id() {
        let mut m = temp_map("lru-live");
        let mut ids = Vec::new();
        for n in 1..=15u8 {
            ids.push(
                m.resolve(&identity_key(fp(n), (1920, 1080), false), &nothing_live())
                    .unwrap(),
            );
        }
        // fp(1) is LRU and live.
        let lru_id = ids[0];
        let live: BTreeSet<u32> = [lru_id].into_iter().collect();
        let id16 = m
            .resolve(&identity_key(fp(16), (1920, 1080), false), &live)
            .expect("14 idle ids remain — one of them is the victim");
        assert_ne!(id16, lru_id, "must not take the id of a live display");
        assert_eq!(id16, ids[1], "the next-least-recently-seen IDLE id instead");
        assert_eq!(
            m.resolve(&identity_key(fp(1), (1920, 1080), false), &live),
            Some(lru_id)
        );
        let _ = std::fs::remove_dir_all(m.path.parent().unwrap());
    }

    #[test]
    fn refuses_rather_than_evicting_when_every_id_is_live() {
        let mut m = temp_map("lru-all-live");
        let mut live = BTreeSet::new();
        for n in 1..=15u8 {
            live.insert(
                m.resolve(&identity_key(fp(n), (1920, 1080), false), &BTreeSet::new())
                    .unwrap(),
            );
        }
        assert_eq!(
            m.resolve(&identity_key(fp(16), (1920, 1080), false), &live),
            None
        );
        assert_eq!(m.store.entries.len(), 15, "nothing was evicted");
        // Known client still resolves: it already owns that id.
        assert!(m
            .resolve(&identity_key(fp(3), (1920, 1080), false), &live)
            .is_some());
        let _ = std::fs::remove_dir_all(m.path.parent().unwrap());
    }

    #[test]
    fn key_composition() {
        assert_eq!(identity_key(fp(0xab), (1920, 1080), false).len(), 64); // hex fp only
        assert!(identity_key(fp(0xab), (1920, 1080), true).ends_with("@1920x1080"));
    }

    #[test]
    fn scale_key_follows_the_identity_policy() {
        use crate::policy::Identity;
        assert_eq!(
            scale_key_for(Identity::Shared, Some(fp(1)), (1920, 1080)),
            "shared"
        );
        assert_eq!(
            scale_key_for(Identity::PerClient, None, (1920, 1080)),
            "shared"
        );
        let pc = scale_key_for(Identity::PerClient, Some(fp(1)), (1920, 1080));
        assert_eq!(pc, identity_key(fp(1), (1920, 1080), false));
        let pcm = scale_key_for(Identity::PerClientMode, Some(fp(1)), (1920, 1080));
        assert!(pcm.ends_with("@1920x1080"));
    }

    #[test]
    fn a_sized_scale_key_names_a_device_and_a_size_or_nothing() {
        use crate::policy::Identity;
        let size = (3840, 2160);
        let sized = identity_key(fp(1), size, true);
        for policy in [Identity::PerClient, Identity::PerClientMode] {
            assert_eq!(
                sized_scale_key_for(policy, Some(fp(1)), size),
                Some(sized.clone())
            );
            assert_eq!(sized_scale_key_for(policy, None, size), None);
        }
        assert_eq!(
            sized_scale_key_for(Identity::Shared, Some(fp(1)), size),
            None
        );
    }

    /// A tablet whose overlay says per-client remembers its own scale on a shared-identity
    /// host: the key follows the same per-device identity the slot does.
    #[test]
    fn scale_key_follows_the_devices_own_identity_overlay() {
        crate::fake::in_child(
            module_path!(),
            "scale_key_follows_the_devices_own_identity_overlay",
            &[],
            || {
                let fp = fp(7);
                let hex = crate::policy::fp_hex(Some(fp)).unwrap();
                let doc = format!(
                    r#"{{"identity":"shared","clients":{{"{hex}":{{"identity":"per-client"}}}}}}"#
                );
                std::fs::write(pf_paths::config_dir().join("display-settings.json"), doc).unwrap();
                let key = scale_key(Some(fp), (1920, 1080), crate::policy::Identity::Shared);
                assert_eq!(
                    key,
                    scale_key_for(crate::policy::Identity::PerClient, Some(fp), (1920, 1080))
                );
            },
        );
    }

    #[test]
    fn one_bad_scale_entry_costs_only_itself() {
        // A hand-edited or half-written value loses its own entry, not every client's scale.
        let dir = std::env::temp_dir().join(format!("pf-scale-bad-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("scale.json");
        std::fs::write(&path, br#"{"a": 1.5, "b": "1.5", "c": 100.0}"#).unwrap();
        let m = ScaleMap::load_from(path);
        assert_eq!(m.get("a"), Some(1.5));
        assert_eq!(m.get("b"), None);
        assert_eq!(m.get("c"), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scale_map_roundtrips_and_rejects_junk() {
        // Its own dir: `set` hardens the parent, which must not be the shared temp root.
        let dir = std::env::temp_dir().join(format!("pf-scale-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("scale.json");
        let mut m = ScaleMap {
            path: path.clone(),
            map: Default::default(),
        };
        assert_eq!(m.get("k"), None);
        m.set("k", 1.5);
        m.set("bad-nan", f64::NAN);
        m.set("bad-range", 100.0);
        assert_eq!(m.get("k"), Some(1.5));
        assert_eq!(m.get("bad-nan"), None);
        assert_eq!(m.get("bad-range"), None);
        let bytes = std::fs::read(&path).unwrap();
        let reread: std::collections::BTreeMap<String, f64> =
            serde_json::from_slice(&bytes).unwrap();
        assert_eq!(reread.get("k"), Some(&1.5));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
