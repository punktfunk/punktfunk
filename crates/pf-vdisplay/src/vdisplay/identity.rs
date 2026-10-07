//! Per-client → stable display-id map. A reconnect reuses the same small id so
//! the compositor can reapply per-display scale. Design: `design/display-management.md`.
//!
//! Ids stay `1..=15` on every platform (Windows IddCx `ConnectorIndex` is
//! `< MaxMonitorsSupported` = 16). The key is cert fingerprint, or fingerprint
//! plus resolution (`per-client-mode`). Sessions with no fingerprint never
//! reach this map (id `0` / auto, upstream).
//!
//! Persist path: `<config>/display-identity.json` (migrates
//! `pf-vdisplay-identity.json`). GNOME cannot rematch a virtual monitor, so
//! [`ScaleMap`] stores scale under the same [`identity_key`].

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

/// Recognizes the seat-session diagnostic marker, not the slot reservation.
pub(crate) fn is_seat_session_marker(value: Option<&OsStr>) -> bool {
    value.and_then(OsStr::to_str) == Some("1")
}

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
}

impl DisplayIdentityMap {
    /// Empty on first run or parse failure (ids re-derive once). Falls back to `pf-vdisplay-identity.json`.
    pub(crate) fn load() -> Self {
        let dir = pf_paths::config_dir();
        let path = dir.join(FILE);
        let (from, bytes) = match std::fs::read(&path) {
            Ok(b) => (path.clone(), Some(b)),
            Err(_) => {
                let legacy = dir.join(LEGACY_FILE);
                match std::fs::read(&legacy) {
                    Ok(b) => (legacy, Some(b)),
                    Err(_) => (path.clone(), None),
                }
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
        Self { path, store }
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

    /// Best-effort [`pf_paths::replace_file`]. Parent is `config_dir()` (host key, allow-list,
    /// mgmt token) so use `create_private_dir` (0700), not `create_dir_all`.
    fn persist(&self) {
        let Ok(bytes) = serde_json::to_vec_pretty(&self.store) else {
            return;
        };
        if let Some(dir) = self.path.parent() {
            let _ = pf_paths::create_private_dir(dir);
        }
        let _ = pf_paths::replace_file(&self.path, &bytes);
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
    let id_policy = crate::policy::prefs()
        .configured()
        .map(|p| {
            p.effective_for(crate::policy::fp_hex(fp).as_deref())
                .identity
        })
        .unwrap_or(default);
    let per_client_mode = match id_policy {
        Identity::Shared => return None,
        Identity::PerClient => false,
        Identity::PerClientMode => true,
    };
    let fp = fp?;
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

/// Scale-map key: [`identity_key`], or `"shared"` for Shared/anonymous.
/// `"shared"` cannot collide — identity keys are 64 hex chars.
pub(crate) fn scale_key(
    fp: Option<[u8; 32]>,
    mode: (u32, u32),
    default: crate::policy::Identity,
) -> String {
    let id_policy = crate::policy::prefs()
        .configured_effective()
        .map(|e| e.identity)
        .unwrap_or(default);
    scale_key_for(id_policy, fp, mode)
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

/// Client-key → desktop-scale. GNOME never rematches `RecordVirtual` EDIDs
/// (fresh serial, no override), so the host stores scale and the Mutter backend
/// reapplies it. Windows/KDE persist scale themselves once the id is stable.
pub(crate) struct ScaleMap {
    path: PathBuf,
    map: std::collections::BTreeMap<String, f64>,
}

impl ScaleMap {
    /// Empty on first run / unreadable. Drop non-finite and values outside 0.25..=8.0
    /// (sane compositor scale range; a hand-edited file can store anything).
    fn load() -> Self {
        let path = pf_paths::config_dir().join(SCALE_FILE);
        let mut map: std::collections::BTreeMap<String, f64> = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        map.retain(|_, s| s.is_finite() && (0.25..=8.0).contains(s));
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
        if let Some(dir) = self.path.parent() {
            // Parent is `config_dir()` (host key, allow-list, token). 0700, not `create_dir_all`.
            let _ = pf_paths::create_private_dir(dir);
        }
        let _ = pf_paths::replace_file(&self.path, &bytes);
    }
}

/// Process-wide scale map, loaded once. Mutter backend only.
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
        }
    }

    fn nothing_live() -> BTreeSet<u32> {
        BTreeSet::new()
    }

    #[test]
    fn seat_session_marker_is_diagnostic_and_exact() {
        assert!(is_seat_session_marker(Some(OsStr::new("1"))));
        for value in [None, Some(OsStr::new("0")), Some(OsStr::new("true"))] {
            assert!(!is_seat_session_marker(value));
        }
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
    fn scale_map_roundtrips_and_rejects_junk() {
        let path = std::env::temp_dir().join(format!("pf-scale-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);
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
        let _ = std::fs::remove_file(&path);
    }
}
