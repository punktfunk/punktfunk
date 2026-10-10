//! Shared native (`punktfunk/1`) pairing state: the on-demand arming PIN, the persistent
//! paired-clients store, and the delegated-approval queue. One [`NativePairing`] handle is
//! shared by the QUIC accept loop ([`crate::native`]) and the management API ([`crate::mgmt`]),
//! so an operator can arm pairing and read the PIN from the web console.
//!
//! The host mints the PIN (SPAKE2); the client enters it. The UI displays a short-lived PIN
//! rather than accepting one.
//!
//! [`NativePairing::add`] pins the fingerprint first, then clears the knock;
//! [`NativePairing::wait_for_decision`] injects an `is_paired` closure into the store-blind
//! approval queue.
//!
//! [`NativePairing::is_paired`] is listing, expiry-blind. [`NativePairing::effective`] is
//! authorization right now (`None` if unpaired or expired). Admission and enforcement use
//! only `effective`. Access mutations publish [`AccessState`] on a per-fingerprint watch;
//! sessions [`NativePairing::subscribe`] at admission. Evidence: the tests below.

use anyhow::Result;
use punktfunk_core::quic::GRANT_ALL;
use std::collections::HashMap;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;
use tokio::sync::watch;

mod approval;
mod arming;
mod sanitize;
mod store;

pub use approval::{classify_source, KnockSource, PairingDecision, PendingRequest};
pub use arming::PinAttempt;
pub use store::{Access, PairedClient};

/// Counts one live session while held. Last-out for a "this session" record
/// starts [`RECONNECT_GRACE`]; a later last-out invalidates that waiter so the
/// window is always from the latest drop. Drop, not a pair of calls: return,
/// error, or cancel all release the count.
///
/// Named apart from `gamelease::SessionGuard` and `session_status::LiveSessionGuard`, which
/// count different things in the same binary.
pub struct SessionCountGuard {
    np: std::sync::Arc<NativePairing>,
    fp_hex: String,
}

impl Drop for SessionCountGuard {
    fn drop(&mut self) {
        let Some(last_out) = self.np.session_ended(&self.fp_hex) else {
            return;
        };
        // Last one out. Drop cannot await; the runtime this session ran on waits
        // the grace. No runtime (shutdown): the record stays until the next last-out.
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let (np, fp_hex) = (self.np.clone(), self.fp_hex.clone());
        handle.spawn(async move { np.drop_session_only_after_grace(&fp_hex, last_out).await });
    }
}

/// Result of a delegated approve. Not a `Result`: a refusal here is an answer about the
/// knock, not a failure of the call.
pub enum ApproveOutcome {
    /// Paired. Carries the stored record as it now stands, which a re-pair may have kept.
    Paired(PairedClient),
    /// No pending request with that id — expired, denied, or already admitted.
    NotFound,
    /// The knock came from the internet, where the operator cannot tell whose it is.
    /// Admit it by arming a PIN bound to its fingerprint instead.
    WanNeedsBoundPin,
}

impl ApproveOutcome {
    /// The stored record, or `None` for either refusal.
    #[cfg(test)]
    pub fn paired(self) -> Option<PairedClient> {
        match self {
            ApproveOutcome::Paired(c) => Some(c),
            ApproveOutcome::NotFound | ApproveOutcome::WanNeedsBoundPin => None,
        }
    }
}

/// What a live session observes about its device's access. Carries the record's raw deadline
/// rather than a pre-evaluated verdict — expiry is checked against the wall clock each time,
/// so "expire now" is a deadline in the past and the session's own deadline task fires on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AccessState {
    /// Masked grant bits. `0` when revoked.
    pub grants: u32,
    /// Unix seconds, host wall clock. `None` = permanent.
    pub deadline_unix: Option<i64>,
    /// Fingerprint is gone from the store. End sessions; do not mute them. Distinct from
    /// view-only (`grants == 0`, `revoked == false`), where the session stays as a spectator.
    pub revoked: bool,
}

/// Shared by the stream marker's quoting and the plugin-text sanitizers.
pub(crate) use sanitize::is_spoofy_char;
/// Stable path for the native accept loop.
pub(crate) use sanitize::sanitize_device_name;

/// How long a "this session" record outlives its last session. A router blip, a client
/// restart or a sleeping laptop must not cost the guest a re-pair; anything longer starts to
/// look like the deadline this grant exists to replace. Deliberately not `gamelease`'s
/// disconnect grace: that one keeps a *game* running, this one keeps *trust*.
#[cfg(not(test))]
pub const RECONNECT_GRACE: Duration = Duration::from_secs(60);
/// The tests exercise the ordering — last session out, reconnect inside the window, nothing
/// coming back — not the length of the wait.
#[cfg(test)]
pub const RECONNECT_GRACE: Duration = Duration::from_millis(60);

pub struct NativePairing {
    arm: arming::ArmState,
    store: store::TrustStore,
    approval: approval::ApprovalQueue,
    /// Fingerprint (lowercased) → live-session channel. Senders stay after unpair so a
    /// late subscriber cannot race a close; a re-pair publishes on the same channel.
    access_watch: Mutex<HashMap<String, watch::Sender<AccessState>>>,
    /// Live session counts plus last-out generation, keyed by lowercased fingerprint.
    /// Every admitted session counts; only a "this session" record reads the total.
    live: Mutex<Live>,
}

/// Session counts and last-out generations behind one lock so a reconnect cannot
/// land between "still live?" and "remove the record".
struct Live {
    counts: HashMap<String, u32>,
    /// Bumped when a fingerprint's count hits zero. A grace waiter drops the
    /// record only if this still matches the generation it started with.
    last_out: HashMap<String, u64>,
}

pub struct NativePairingStatus {
    pub armed: bool,
    pub pin: Option<String>,
    /// Seconds left. `None` = no expiry (CLI `--allow-pairing`).
    pub expires_in_secs: Option<u64>,
    pub paired_clients: u32,
}

impl NativePairing {
    /// Load the trust store. `store_path = None` uses the default path. `arm_at_start` (CLI
    /// `--allow-pairing` / `--require-pairing`) arms immediately with `fixed_pin` or a random
    /// PIN and no expiry.
    pub fn load_with(
        store_path: Option<PathBuf>,
        fixed_pin: Option<String>,
        arm_at_start: bool,
    ) -> Result<NativePairing> {
        let store = store::TrustStore::open(store_path)?;
        let refused = pf_paths::seat::pairing_refused() || store.read_only();
        let np = NativePairing {
            arm: arming::ArmState::new(arm_at_start && !refused, fixed_pin),
            store,
            approval: approval::ApprovalQueue::new(),
            access_watch: Mutex::new(HashMap::new()),
            live: Mutex::new(Live {
                counts: HashMap::new(),
                last_out: HashMap::new(),
            }),
        };
        np.drop_session_only_records();
        Ok(np)
    }

    /// A host that is starting has no live sessions, so every "this session" grant has already
    /// ended — by a restart, an update or a crash, which is how a session usually ends. The
    /// counter is in memory only, so without this sweep those records would outlive the guest
    /// forever and the list would keep exactly the leftover the grant exists to prevent.
    fn drop_session_only_records(&self) {
        if self.store.read_only() {
            return;
        }
        for client in self.store.list() {
            if !client.until_disconnect {
                continue;
            }
            match self.store.remove(&client.fingerprint) {
                Ok(true) => tracing::info!(
                    name = %client.name,
                    fingerprint = %client.fingerprint,
                    "dropped a this-session grant left by the previous run"
                ),
                Ok(false) => {}
                Err(e) => tracing::warn!(
                    error = %e,
                    fingerprint = %client.fingerprint,
                    "couldn't drop a this-session grant at startup"
                ),
            }
        }
    }

    /// Arm with a fresh PIN for `ttl`, unbound (any well-formed attempt consumes it) and with
    /// no access choice. Prefer [`Self::arm_for`] on untrusted LANs — an unbound window is
    /// burnable by any peer.
    #[cfg(test)]
    pub fn arm(&self, ttl: Duration) -> String {
        self.arm.arm_for(ttl, None, None)
    }

    /// Arm with a fresh PIN for `ttl`. `bound_fp` restricts consumption to that fingerprint;
    /// another peer can neither pair nor burn the window. `access` is the operator's grant
    /// for whoever completes the ceremony; `None` = full/permanent default.
    pub fn arm_for(
        &self,
        ttl: Duration,
        bound_fp: Option<String>,
        access: Option<Access>,
    ) -> String {
        self.arm.arm_for(ttl, bound_fp, access)
    }

    /// Access choice on the current window (`None` if disarmed, expired, or unset). The PIN
    /// ceremony reads this before consuming the window; [`Self::disarm`] wipes it with the PIN.
    pub fn armed_access(&self) -> Option<Access> {
        self.arm.armed_access()
    }

    /// PIN for an attempt from `client_fp_hex` knocking from `source`. `BoundToOther` and
    /// `UnboundForWan` both mean reject without consuming the window; `Disarmed` means none
    /// is armed.
    pub fn pin_for_attempt(&self, client_fp_hex: &str, source: KnockSource) -> PinAttempt {
        if self.pairing_refused() {
            return PinAttempt::Disarmed;
        }
        self.arm.pin_for_attempt(client_fp_hex, source)
    }

    /// A seat host: devices pair with the box, so no PIN window opens and no knock parks.
    pub fn pairing_refused(&self) -> bool {
        pf_paths::seat::pairing_refused() || self.store.read_only()
    }

    pub fn disarm(&self) {
        self.arm.disarm()
    }

    /// Current valid PIN, or `None` if disarmed/expired. Read per attempt so a window that
    /// lapses mid-connection no longer pairs.
    pub fn current_pin(&self) -> Option<String> {
        self.arm.current_pin()
    }

    pub fn status(&self) -> NativePairingStatus {
        let (armed, pin, expires_in_secs) = self.arm.snapshot();
        NativePairingStatus {
            armed,
            pin,
            expires_in_secs,
            paired_clients: self.store.count(),
        }
    }

    /// Listed in the paired set, expiry-blind. An expired guest still shows in the device list
    /// and still short-circuits the approval queue. Admission and enforcement use
    /// [`Self::effective`].
    #[cfg(test)]
    pub fn is_paired(&self, fp_hex: &str) -> bool {
        self.store.is_paired(fp_hex)
    }

    /// Grant mask authorized at `now_unix` (host wall clock, unix seconds): `None` if unpaired
    /// or expired. Absent grants mean full control; reserved bits are already masked.
    pub fn effective(&self, fp_hex: &str, now_unix: i64) -> Option<u32> {
        self.store.effective(fp_hex, now_unix)
    }

    /// GameStream grant for this fingerprint. No row means ungoverned full control — that plane's
    /// pairing authority is its cert list, not this store. A row that exists governs as native
    /// (`effective`): mask, reserved bits cleared, expiry → `None`. One snapshot: `is_paired`
    /// then `effective` can race a delete into "listed but expired" for a just-ungoverned row.
    #[cfg_attr(not(feature = "gamestream"), allow(dead_code, reason = "compat plane"))]
    pub fn moonlight_effective(&self, fp_hex: &str, now_unix: i64) -> Option<u32> {
        match self.store.get(fp_hex) {
            None => Some(GRANT_ALL),
            Some(c) => {
                if c.expires_unix.is_some_and(|t| now_unix >= t) {
                    None
                } else {
                    Some(
                        punktfunk_core::quic::normalize_legacy_full(c.grants.unwrap_or(GRANT_ALL))
                            & GRANT_ALL,
                    )
                }
            }
        }
    }

    /// Record a successful pairing with no explicit access choice. New fingerprint: full
    /// permanent default. Existing: name-only — grants and expiry stay, so a limited guest
    /// cannot re-pair itself to full control. Widening goes through [`Self::add_with_access`],
    /// [`Self::set_access`], or [`Self::approve_pending`].
    #[cfg(test)]
    pub fn add(&self, name: &str, fp_hex: &str) -> Result<()> {
        self.add_with_access(name, fp_hex, None)
    }

    /// Record a successful pairing. `Some(access)` replaces the record; `None` preserves as
    /// [`Self::add`]. Persist failure rolls the in-memory store back. Pins first, then clears
    /// the knock and wakes waiters — [`Self::wait_for_decision`] requires a woken waiter to
    /// observe paired and no longer pending — then publishes to live watchers.
    pub fn add_with_access(&self, name: &str, fp_hex: &str, access: Option<Access>) -> Result<()> {
        self.store.add_with_access(name, fp_hex, access)?;
        self.approval.admit_and_clear(fp_hex);
        // Every successful pairing (PIN and delegated approval) passes here, so
        // `pairing.completed` fires once.
        let device = crate::events::DeviceRef {
            name: sanitize_device_name(name, fp_hex),
            fingerprint: fp_hex.to_string(),
            plane: crate::events::Plane::Native,
        };
        crate::events::emit(crate::events::EventKind::PairingCompleted {
            device: device.clone(),
        });
        // `access.granted` only for an explicit operator choice (approve dialog, arm
        // window). Read the stored record: reserved bits are already masked and grant
        // time stamped. A choice-less pairing emits only `pairing.completed` above.
        if access.is_some() {
            if let Some(stored) = self.store.get(fp_hex) {
                crate::events::emit(crate::events::EventKind::AccessGranted {
                    device,
                    grants: stored.grants.unwrap_or(GRANT_ALL) & GRANT_ALL,
                    expires_unix: stored.expires_unix,
                });
            }
        }
        self.publish_current(fp_hex);
        Ok(())
    }

    /// Overwrite a paired device's access, persist, then publish to live sessions in one
    /// watch event. Returns `false` (no write, no publish) for an unknown fingerprint —
    /// editing access is not a way to pair.
    pub fn set_access(&self, fp_hex: &str, access: Access) -> Result<bool> {
        if !self.store.set_access(fp_hex, access)? {
            return Ok(false);
        }
        // Read the stored record so hooks see masked bits actually in force.
        if let Some(stored) = self.store.get(fp_hex) {
            crate::events::emit(crate::events::EventKind::AccessChanged {
                device: crate::events::DeviceRef {
                    name: stored.name,
                    fingerprint: fp_hex.to_ascii_lowercase(),
                    plane: crate::events::Plane::Native,
                },
                grants: stored.grants.unwrap_or(GRANT_ALL) & GRANT_ALL,
                expires_unix: stored.expires_unix,
            });
        }
        self.publish_current(fp_hex);
        Ok(true)
    }

    /// Subscribe to this fingerprint's access. Current value is the state now (unpaired ⇒
    /// already `revoked`); later pair/edit/unpair arrive as change notifications.
    pub fn subscribe(&self, fp_hex: &str) -> watch::Receiver<AccessState> {
        let mut map = self.access_watch.lock().unwrap();
        map.entry(fp_hex.to_ascii_lowercase())
            .or_insert_with(|| watch::channel(self.current_state(fp_hex)).0)
            .subscribe()
    }

    fn current_state(&self, fp_hex: &str) -> AccessState {
        match self.store.get(fp_hex) {
            Some(c) => AccessState {
                grants: c.grants.unwrap_or(GRANT_ALL) & GRANT_ALL,
                deadline_unix: c.expires_unix,
                revoked: false,
            },
            None => AccessState {
                grants: 0,
                deadline_unix: None,
                revoked: true,
            },
        }
    }

    /// Publish `fp_hex` to its watchers. No-op if nobody subscribed and nothing mutated it:
    /// the channel is minted lazily. Same-value mutations wake nobody.
    fn publish_current(&self, fp_hex: &str) {
        let state = self.current_state(fp_hex);
        let mut map = self.access_watch.lock().unwrap();
        let tx = map
            .entry(fp_hex.to_ascii_lowercase())
            .or_insert_with(|| watch::channel(state).0);
        tx.send_if_modified(|cur| {
            if *cur == state {
                false
            } else {
                *cur = state;
                true
            }
        });
    }

    pub fn list(&self) -> Vec<PairedClient> {
        self.store.list()
    }

    /// Player slot this device's pads take, 0-based. `None` = the lazy claim. Expiry-blind:
    /// it places controllers, so an expired record's pick is simply not reached.
    pub fn pad_slot_of(&self, fp_hex: &str) -> Option<u8> {
        self.store.get(fp_hex)?.preferred_pad_slot
    }

    /// Remember which player this device is, so its next connect lands on the same slot.
    /// `false` (no write) for an unknown fingerprint.
    pub fn set_pad_slot(&self, fp_hex: &str, slot: Option<u8>) -> Result<bool> {
        self.store.set_pad_slot(fp_hex, slot)
    }

    /// Hold a live-session count for `fp_hex` until the guard drops. Every admitted
    /// session counts; only a "this session" record reads the total.
    pub fn session_started(self: &std::sync::Arc<Self>, fp_hex: &str) -> SessionCountGuard {
        *self
            .live
            .lock()
            .unwrap()
            .counts
            .entry(fp_hex.to_ascii_lowercase())
            .or_insert(0) += 1;
        SessionCountGuard {
            np: self.clone(),
            fp_hex: fp_hex.to_ascii_lowercase(),
        }
    }

    /// Live sessions for `fp_hex` right now.
    #[cfg(test)]
    pub fn live_sessions(&self, fp_hex: &str) -> u32 {
        self.live
            .lock()
            .unwrap()
            .counts
            .get(&fp_hex.to_ascii_lowercase())
            .copied()
            .unwrap_or(0)
    }

    /// Drop one live session. `Some(n)` is last-out: the grace waiter must
    /// carry that generation so a later flap owns the window.
    fn session_ended(&self, fp_hex: &str) -> Option<u64> {
        let mut live = self.live.lock().unwrap();
        let key = fp_hex.to_ascii_lowercase();
        let n = live.counts.get_mut(&key)?;
        *n = n.saturating_sub(1);
        if *n > 0 {
            return None;
        }
        live.counts.remove(&key);
        let last_out = live.last_out.entry(key).or_insert(0);
        *last_out += 1;
        Some(*last_out)
    }

    /// Wait [`RECONNECT_GRACE`], then drop a "this session" record if this last-out
    /// is still the latest and the device did not come back. `session_started` and
    /// this waiter share the `live` lock so a reconnect cannot land between the live
    /// check and the remove. Lock order is live → store → watch.
    async fn drop_session_only_after_grace(&self, fp_hex: &str, last_out: u64) {
        tokio::time::sleep(RECONNECT_GRACE).await;
        let removed = {
            let live = self.live.lock().unwrap();
            let key = fp_hex.to_ascii_lowercase();
            if live.counts.contains_key(&key) {
                return;
            }
            if live.last_out.get(&key).copied() != Some(last_out) {
                return;
            }
            match self.store.get(fp_hex) {
                Some(record) if record.until_disconnect => {
                    Some((record.name, self.store.remove(fp_hex)))
                }
                _ => return,
            }
        };
        let Some((name, removed)) = removed else {
            return;
        };
        match removed {
            Ok(true) => {
                self.publish_current(fp_hex);
                tracing::info!(
                    %name,
                    fingerprint = %fp_hex,
                    "this-session access ended with the device's last session"
                );
            }
            Ok(false) => {}
            // The record stays and the operator can still unpair by hand; keeping a row too
            // long is the safe direction to fail in, not one to panic over.
            Err(e) => tracing::warn!(
                error = %e,
                fingerprint = %fp_hex,
                "couldn't drop a this-session record"
            ),
        }
    }

    /// Remove by fingerprint. Persist failure rolls the in-memory store back. A removal
    /// publishes `revoked` so live sessions can end themselves.
    pub fn remove(&self, fp_hex: &str) -> Result<bool> {
        let removed = self.store.remove(fp_hex)?;
        if removed {
            self.publish_current(fp_hex);
        }
        Ok(removed)
    }

    /// Remove every paired client in one write. Returns the fingerprints so the caller can
    /// end those sessions. Persist failure removes nothing. Publishes `revoked` per row.
    pub fn remove_all(&self) -> Result<Vec<String>> {
        let removed = self.store.remove_all()?;
        for fp in &removed {
            self.publish_current(fp);
        }
        Ok(removed)
    }

    /// Record an unpaired knock. A re-knock from the same fingerprint refreshes in place
    /// (same id, new generation). The generation is what [`Self::wait_for_decision`] admits.
    pub fn note_pending(
        &self,
        name: &str,
        fp_hex: &str,
        src_ip: Option<IpAddr>,
        profile: Option<&str>,
    ) -> u32 {
        // Only a new fingerprint emits `pairing.pending`. A parked client's retries
        // must not notify the operator once per attempt.
        let was_pending = self.approval.pending_contains(fp_hex);
        let seq = self.approval.note_pending(name, fp_hex, src_ip, profile);
        if !was_pending {
            crate::events::emit(crate::events::EventKind::PairingPending {
                device: crate::events::DeviceRef {
                    name: sanitize_device_name(name, fp_hex),
                    fingerprint: fp_hex.to_string(),
                    plane: crate::events::Plane::Native,
                },
            });
        }
        seq
    }

    pub fn pending(&self) -> Vec<PendingRequest> {
        self.approval.pending()
    }

    /// Drops expired entries first.
    #[cfg(test)]
    pub fn pending_contains(&self, fp_hex: &str) -> bool {
        self.approval.pending_contains(fp_hex)
    }

    /// Approve a pending knock: pair under `name_override` (else the knock's name) and drop
    /// the queue entry. `access` is the dialog choice; `None` keeps existing access or the
    /// full/permanent default. `NotFound` = unknown or expired id. Reads the entry (does not
    /// pre-remove), then [`Self::add_with_access`] pins and clears — waiters rely on that order.
    pub fn approve_pending(
        &self,
        id: u32,
        name_override: Option<&str>,
        access: Option<Access>,
    ) -> Result<ApproveOutcome> {
        // The pending card shows a name the device chose for itself, so from the internet a
        // one-click approve is a guess about who knocked. Only a PIN window bound to this
        // fingerprint can admit one. Read before the entry, not after: an entry that expires
        // between the two lookups must read as gone, never as a knock with no source.
        match self.approval.source_of(id) {
            None => return Ok(ApproveOutcome::NotFound),
            Some(KnockSource::Wan) => return Ok(ApproveOutcome::WanNeedsBoundPin),
            Some(KnockSource::Lan) => {}
        }
        let (knock_name, fp_hex) = match self.approval.read_entry(id) {
            Some(x) => x,
            None => return Ok(ApproveOutcome::NotFound),
        };
        let name = name_override.unwrap_or(&knock_name).to_string();
        self.add_with_access(&name, &fp_hex, access)?;

        // Read the stored record: `access == None` on a re-pair kept prior grants/expiry.
        Ok(match self.store.get(&fp_hex) {
            Some(c) => ApproveOutcome::Paired(c),
            None => ApproveOutcome::NotFound,
        })
    }

    /// Deny is "not now": the next knock creates a new entry.
    pub fn deny_pending(&self, id: u32) -> bool {
        // Identity for the lifecycle event; deny after this would lose the name.
        let entry = self.approval.read_entry(id);
        let denied = self.approval.deny_pending(id);
        if denied {
            if let Some((name, fp_hex)) = entry {
                crate::events::emit(crate::events::EventKind::PairingDenied {
                    device: crate::events::DeviceRef {
                        name: sanitize_device_name(&name, &fp_hex),
                        fingerprint: fp_hex,
                        plane: crate::events::Plane::Native,
                    },
                });
            }
        }
        denied
    }

    /// Park until an operator decides on `fp_hex`, up to `timeout`. `knock_seq` is the
    /// generation [`Self::note_pending`] returned for this connection. The queue is store-blind;
    /// the paired-check closure resolves [`PairingDecision::Approved`] the instant the
    /// fingerprint is authorized. See [`approval::ApprovalQueue::wait_for_decision`].
    ///
    /// The closure uses [`Self::effective`], not [`Self::is_paired`]: an expired guest is still
    /// listed, and resolving on listing would admit the knock before re-grant, then fail
    /// admission with a typed expiry close. A bare PIN re-pair preserves expired access and
    /// does not admit a parked knock either.
    pub async fn wait_for_decision(
        &self,
        fp_hex: &str,
        knock_seq: u32,
        timeout: Duration,
    ) -> PairingDecision {
        self.approval
            .wait_for_decision(fp_hex, knock_seq, timeout, |fp| {
                self.store
                    .effective(fp, crate::clock::unix_secs())
                    .is_some()
            })
            .await
    }

    /// Test-only park flag. Behavior tests assert a parked knock survives a flood.
    #[cfg(test)]
    fn set_parked(&self, fp_hex: &str, knock_seq: u32, parked: bool) {
        self.approval.set_parked(fp_hex, knock_seq, parked)
    }
}

#[cfg(test)]
mod tests {
    /// Knocks in these tests come from the couch unless a test says otherwise. `None` now
    /// classifies as WAN, which refuses a bare approve — that is the point of the rule.
    const LAN_KNOCK: std::net::IpAddr =
        std::net::IpAddr::V4(std::net::Ipv4Addr::new(192, 168, 1, 44));

    use super::approval::{MAX_PENDING_PER_IP, PENDING_CAP};
    use super::*;

    /// The dir is returned with the path: dropping it deletes the store.
    fn temp() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("paired.json");
        (dir, path)
    }

    #[test]
    fn arm_expire_and_pair() {
        let (_temp, p) = temp();
        let np = NativePairing::load_with(Some(p.clone()), None, false).unwrap();
        assert!(np.current_pin().is_none());
        assert!(!np.status().armed);

        let pin = np.arm(Duration::from_millis(40));
        assert_eq!(pin.len(), 4);
        assert_eq!(np.current_pin().as_deref(), Some(pin.as_str()));
        assert!(np.status().armed);
        std::thread::sleep(Duration::from_millis(60));
        assert!(np.current_pin().is_none(), "window should have expired");
        assert!(!np.status().armed);

        assert!(!np.is_paired("ab12"));
        np.add("Living Room", "AB12").unwrap();
        assert!(
            np.is_paired("ab12"),
            "fingerprint match is case-insensitive"
        );
        assert_eq!(np.list().len(), 1);
        assert_eq!(np.status().paired_clients, 1);
        assert!(np.remove("ab12").unwrap());
        assert!(!np.remove("ab12").unwrap());
        assert!(np.list().is_empty());
    }

    #[test]
    fn pending_knock_approve_and_deny() {
        let (_temp, p) = temp();
        let np = NativePairing::load_with(Some(p.clone()), None, false).unwrap();
        assert!(np.pending().is_empty());

        np.note_pending("device aa11", "AA11", Some(LAN_KNOCK), None);
        np.note_pending("Bedroom TV", "aa11", Some(LAN_KNOCK), None);
        let pend = np.pending();
        assert_eq!(pend.len(), 1, "re-knock dedups by fingerprint");
        assert_eq!(pend[0].name, "Bedroom TV");
        let id = pend[0].id;

        assert!(np.deny_pending(id));
        assert!(!np.deny_pending(id));
        assert!(np.pending().is_empty());
        assert!(!np.is_paired("aa11"));

        np.note_pending("device bb22", "BB22", Some(LAN_KNOCK), None);
        let id = np.pending()[0].id;
        assert!(
            np.approve_pending(9999, None, None)
                .unwrap()
                .paired()
                .is_none(),
            "unknown id"
        );
        let client = np
            .approve_pending(id, Some("Living Room"), None)
            .unwrap()
            .paired()
            .unwrap();
        assert_eq!(client.name, "Living Room");
        assert!(np.is_paired("bb22"), "approval pins the fingerprint");
        assert!(np.pending().is_empty());
        assert_eq!(np.list()[0].name, "Living Room");

        // Distinct source IPs so the per-IP cap does not fire; the global cap holds
        // at PENDING_CAP and evicts the oldest non-parked entries first.
        for i in 0..(PENDING_CAP + 3) {
            let ip = IpAddr::from([10, 0, (i / 256) as u8, (i % 256) as u8]);
            np.note_pending("flood", &format!("f{i:03}"), Some(ip), None);
        }
        let pend = np.pending();
        assert_eq!(pend.len(), PENDING_CAP);
        assert_eq!(pend[0].fingerprint, "f003", "oldest entries evicted first");
    }

    #[test]
    fn pairing_clears_a_pending_knock() {
        let (_temp, p) = temp();
        let np = NativePairing::load_with(Some(p.clone()), None, false).unwrap();
        np.note_pending("Knocker", "cc44", Some(LAN_KNOCK), None);
        assert_eq!(np.pending().len(), 1);
        np.add("Knocker", "CC44").unwrap();
        assert!(
            np.pending().is_empty(),
            "a now-paired device must leave the approval list"
        );
        assert!(np.is_paired("cc44"));
    }

    #[test]
    fn add_replaces_case_insensitively() {
        let (_temp, p) = temp();
        let np = NativePairing::load_with(Some(p.clone()), None, false).unwrap();
        np.add("First", "AB12").unwrap();
        np.add("Second", "ab12").unwrap();
        assert_eq!(np.list().len(), 1, "re-add must replace, not duplicate");
        assert_eq!(np.list()[0].name, "Second");
    }

    #[test]
    fn cli_flag_arms_with_no_expiry() {
        let (_temp, p) = temp();
        let np = NativePairing::load_with(Some(p.clone()), Some("1234".into()), true).unwrap();
        assert_eq!(np.current_pin().as_deref(), Some("1234"));
        let s = np.status();
        assert!(s.armed);
        assert_eq!(s.expires_in_secs, None, "CLI arming has no expiry");
        np.disarm();
        assert!(np.current_pin().is_none());
    }

    #[tokio::test]
    async fn wait_for_decision_approve_deny_timeout() {
        use std::sync::Arc;
        let (_temp, p) = temp();
        let np = Arc::new(NativePairing::load_with(Some(p.clone()), None, false).unwrap());

        let seq = np.note_pending("Knocker", "ab01", Some(LAN_KNOCK), None);
        let d = np
            .wait_for_decision("ab01", seq, Duration::from_millis(80))
            .await;
        assert_eq!(d, PairingDecision::TimedOut);
        assert!(np.pending_contains("ab01"));

        let ready = np.approval.waiter_ready_generation();
        let np2 = np.clone();
        let waiter = tokio::spawn(async move {
            np2.wait_for_decision("ab01", seq, Duration::from_secs(5))
                .await
        });
        np.approval.wait_for_waiter_ready(ready).await;
        let id = np
            .pending()
            .into_iter()
            .find(|x| x.fingerprint == "ab01")
            .unwrap()
            .id;
        np.approve_pending(id, Some("Approved"), None)
            .unwrap()
            .paired()
            .unwrap();
        assert_eq!(waiter.await.unwrap(), PairingDecision::Approved);
        assert!(np.is_paired("ab01"));

        let seq = np.note_pending("Knock2", "cd02", Some(LAN_KNOCK), None);
        let ready = np.approval.waiter_ready_generation();
        let np3 = np.clone();
        let waiter = tokio::spawn(async move {
            np3.wait_for_decision("cd02", seq, Duration::from_secs(5))
                .await
        });
        np.approval.wait_for_waiter_ready(ready).await;
        let id = np
            .pending()
            .into_iter()
            .find(|x| x.fingerprint == "cd02")
            .unwrap()
            .id;
        assert!(np.deny_pending(id));
        assert_eq!(waiter.await.unwrap(), PairingDecision::Denied);
        assert!(!np.is_paired("cd02"));

        // Already paired (PIN-ceremony race): generation 0 matches a coincidental waiter
        // and resolves Approved immediately.
        let d = np
            .wait_for_decision("ab01", 0, Duration::from_secs(5))
            .await;
        assert_eq!(d, PairingDecision::Approved);
    }

    /// An expired record is still listed. A paired-check on listing would admit the knock
    /// before re-grant; the session would then die on admission's effective-check. Only
    /// operator re-approval (which refreshes access) may resolve the waiter.
    #[tokio::test]
    async fn expired_record_parks_until_regrant() {
        use punktfunk_core::quic::GRANT_GAMEPAD;
        use std::sync::Arc;
        let (_temp, p) = temp();
        let np = Arc::new(NativePairing::load_with(Some(p.clone()), None, false).unwrap());
        np.add_with_access(
            "Old Guest",
            "aa77",
            Some(Access {
                grants: GRANT_ALL,
                expires_unix: Some(wall_now() - 10),
                until_disconnect: false,
            }),
        )
        .unwrap();
        assert!(np.is_paired("aa77"), "expired but still listed");

        let seq = np.note_pending("Old Guest", "aa77", Some(LAN_KNOCK), None);
        let d = np
            .wait_for_decision("aa77", seq, Duration::from_millis(120))
            .await;
        assert_eq!(d, PairingDecision::TimedOut);

        let seq = np.note_pending("Old Guest", "aa77", Some(LAN_KNOCK), None);
        let ready = np.approval.waiter_ready_generation();
        let np2 = np.clone();
        let waiter = tokio::spawn(async move {
            np2.wait_for_decision("aa77", seq, Duration::from_secs(5))
                .await
        });
        np.approval.wait_for_waiter_ready(ready).await;
        let id = np
            .pending()
            .into_iter()
            .find(|x| x.fingerprint == "aa77")
            .unwrap()
            .id;
        np.approve_pending(
            id,
            None,
            Some(Access {
                grants: GRANT_GAMEPAD,
                expires_unix: Some(wall_now() + 3600),
                until_disconnect: false,
            }),
        )
        .unwrap()
        .paired()
        .unwrap();
        assert_eq!(waiter.await.unwrap(), PairingDecision::Approved);
        assert_eq!(np.effective("aa77", wall_now()), Some(GRANT_GAMEPAD));
    }

    /// One Approve admits exactly one session. A re-knock supersedes the previous parked
    /// waiter immediately. A stale-generation waiter that polls only after approval still
    /// resolves `Superseded` off the admitted marker.
    #[tokio::test]
    async fn newest_knock_supersedes_parked_waiter() {
        use std::sync::Arc;
        let (_temp, p) = temp();
        let np = Arc::new(NativePairing::load_with(Some(p.clone()), None, false).unwrap());

        let seq1 = np.note_pending("iPad Pro", "ee01", Some(LAN_KNOCK), None);
        let ready = np.approval.waiter_ready_generation();
        let np1 = np.clone();
        let waiter1 = tokio::spawn(async move {
            np1.wait_for_decision("ee01", seq1, Duration::from_secs(5))
                .await
        });
        np.approval.wait_for_waiter_ready(ready).await;

        let seq2 = np.note_pending("iPad Pro", "ee01", Some(LAN_KNOCK), None);
        assert_ne!(seq1, seq2);
        assert_eq!(waiter1.await.unwrap(), PairingDecision::Superseded);
        assert_eq!(np.pending().len(), 1);

        let ready = np.approval.waiter_ready_generation();
        let np2 = np.clone();
        let waiter2 = tokio::spawn(async move {
            np2.wait_for_decision("ee01", seq2, Duration::from_secs(5))
                .await
        });
        np.approval.wait_for_waiter_ready(ready).await;
        let id = np
            .pending()
            .into_iter()
            .find(|x| x.fingerprint == "ee01")
            .unwrap()
            .id;
        np.approve_pending(id, None, None)
            .unwrap()
            .paired()
            .unwrap();
        assert_eq!(waiter2.await.unwrap(), PairingDecision::Approved);

        // After approval the entry is gone and the fingerprint is paired; the admitted
        // marker must still resolve this stale generation as Superseded, not Approved.
        let d = np
            .wait_for_decision("ee01", seq1, Duration::from_millis(80))
            .await;
        assert_eq!(d, PairingDecision::Superseded);
    }

    /// A "this session" record goes when the device's last session ends — but not while another
    /// is live, and not if the device comes back inside the grace. Real clock against the short
    /// test grace: `tokio`'s `test-util` is not enabled here, so there is none to pause. The
    /// reconnect leg takes its guard at once rather than sleeping into the window, which would
    /// race the timer on a loaded runner.
    #[test]
    fn this_session_access_ends_with_the_last_session() {
        use punktfunk_core::quic::GRANT_GAMEPAD;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let (_temp, path) = temp();
            let np =
                std::sync::Arc::new(NativePairing::load_with(Some(path), None, false).unwrap());
            np.add_with_access(
                "Friend's Deck",
                "dd11",
                Some(Access {
                    grants: GRANT_GAMEPAD,
                    expires_unix: None,
                    until_disconnect: true,
                }),
            )
            .unwrap();

            // Two overlapping sessions: the first ending is not the last one out.
            let first = np.session_started("dd11");
            let second = np.session_started("dd11");
            assert_eq!(np.live_sessions("dd11"), 2);
            drop(first);
            assert_eq!(np.live_sessions("dd11"), 1);
            tokio::time::sleep(RECONNECT_GRACE * 2).await;
            assert!(
                np.is_paired("dd11"),
                "another live session holds the record"
            );

            // Last one out, then a reconnect inside the grace — taken at once, so no amount of
            // scheduler stall can put it on the wrong side of the timer.
            drop(second);
            let reconnect = np.session_started("dd11");
            tokio::time::sleep(RECONNECT_GRACE * 2).await;
            assert!(
                np.is_paired("dd11"),
                "a device back inside the grace keeps its access"
            );

            // Nothing comes back this time.
            drop(reconnect);
            tokio::time::sleep(RECONNECT_GRACE * 2).await;
            assert!(
                !np.is_paired("dd11"),
                "the record goes with the last session"
            );
            assert_eq!(np.effective("dd11", wall_now()), None);
        });
    }

    /// A this-session guest who reconnects and drops again gets a full grace from
    /// the latest drop. The first last-out's timer must not revoke them at the
    /// original deadline — a Wi-Fi flap that recovers then dies again.
    #[test]
    fn this_session_grace_restarts_from_the_latest_drop() {
        use punktfunk_core::quic::GRANT_GAMEPAD;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let (_temp, path) = temp();
            let np =
                std::sync::Arc::new(NativePairing::load_with(Some(path), None, false).unwrap());
            np.add_with_access(
                "Friend's Deck",
                "aa33",
                Some(Access {
                    grants: GRANT_GAMEPAD,
                    expires_unix: None,
                    until_disconnect: true,
                }),
            )
            .unwrap();

            let first = np.session_started("aa33");
            drop(first);
            // Hold the reconnect across a slice of the first window so that window's
            // waiter is in flight when this drop starts a second one.
            let reconnect = np.session_started("aa33");
            tokio::time::sleep(RECONNECT_GRACE / 2).await;
            drop(reconnect);
            // First waiter is due; second still has a slice of grace left.
            tokio::time::sleep(RECONNECT_GRACE * 3 / 4).await;
            assert!(
                np.is_paired("aa33"),
                "grace is measured from the latest drop, not the first last-out"
            );
            tokio::time::sleep(RECONNECT_GRACE * 2).await;
            assert!(
                !np.is_paired("aa33"),
                "the record goes once the latest window closes"
            );
        });
    }

    /// The flag is what does it: an ordinary grant outlives its sessions, and a PATCH that only
    /// moves the expiry must not turn a this-session grant permanent.
    #[test]
    fn a_dated_grant_outlives_its_sessions() {
        use punktfunk_core::quic::GRANT_GAMEPAD;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let (_temp, path) = temp();
            let np =
                std::sync::Arc::new(NativePairing::load_with(Some(path), None, false).unwrap());
            np.add_with_access(
                "Living Room",
                "ee22",
                Some(Access {
                    grants: GRANT_GAMEPAD,
                    expires_unix: Some(wall_now() + 3600),
                    until_disconnect: false,
                }),
            )
            .unwrap();
            drop(np.session_started("ee22"));
            tokio::time::sleep(RECONNECT_GRACE * 2).await;
            assert!(np.is_paired("ee22"), "a dated grant is not a session grant");

            // Re-granting as "this session" now behaves like one.
            np.set_access(
                "ee22",
                Access {
                    grants: GRANT_GAMEPAD,
                    expires_unix: None,
                    until_disconnect: true,
                },
            )
            .unwrap();
            drop(np.session_started("ee22"));
            tokio::time::sleep(RECONNECT_GRACE * 2).await;
            assert!(!np.is_paired("ee22"));
        });
    }

    /// A device that knocked from the couch and came back from the internet must be reclassified.
    /// The entry refreshes in place for up to the pending TTL, so a stale source would let a bare
    /// approve admit exactly the knock the rule exists to refuse.
    #[test]
    fn a_re_knock_is_classified_by_where_it_knocked_from() {
        let (_temp, p) = temp();
        let np = NativePairing::load_with(Some(p), None, false).unwrap();
        let wan = std::net::IpAddr::V4(std::net::Ipv4Addr::new(203, 0, 113, 5));

        np.note_pending("Roaming Laptop", "ab99", Some(LAN_KNOCK), None);
        assert_eq!(np.pending()[0].source, KnockSource::Lan);
        np.note_pending("Roaming Laptop", "ab99", Some(wan), None);
        assert_eq!(
            np.pending()[0].source,
            KnockSource::Wan,
            "the entry refreshed in place, so its source must refresh too"
        );
        let id = np.pending()[0].id;
        assert!(matches!(
            np.approve_pending(id, None, None).unwrap(),
            ApproveOutcome::WanNeedsBoundPin
        ));

        // And back again, so the honest direction is not a one-way door.
        np.note_pending("Roaming Laptop", "ab99", Some(LAN_KNOCK), None);
        assert_eq!(np.pending()[0].source, KnockSource::Lan);
    }

    /// A host that is starting has no sessions, so every "this session" grant already ended —
    /// most often by the restart itself. Without the sweep the record would outlive the guest,
    /// because the live count lives only in memory.
    #[test]
    fn a_restart_drops_this_session_grants() {
        use punktfunk_core::quic::GRANT_GAMEPAD;
        let (_temp, path) = temp();
        {
            let np = NativePairing::load_with(Some(path.clone()), None, false).unwrap();
            np.add_with_access(
                "Friend's Deck",
                "dd11",
                Some(Access {
                    grants: GRANT_GAMEPAD,
                    expires_unix: None,
                    until_disconnect: true,
                }),
            )
            .unwrap();
            np.add_with_access(
                "Living Room",
                "ee22",
                Some(Access {
                    grants: GRANT_GAMEPAD,
                    expires_unix: None,
                    until_disconnect: false,
                }),
            )
            .unwrap();
            assert!(np.is_paired("dd11") && np.is_paired("ee22"));
        }

        let restarted = NativePairing::load_with(Some(path), None, false).unwrap();
        assert!(
            !restarted.is_paired("dd11"),
            "the guest's session ended when the host did"
        );
        assert!(
            restarted.is_paired("ee22"),
            "an ordinary grant survives a restart"
        );
    }

    /// The address table the admission rules read. Anything not on a network the operator
    /// already let the peer onto is WAN, and an address we could not read is WAN too.
    #[test]
    fn knock_sources_classify_by_address() {
        use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
        let lan: [IpAddr; 6] = [
            Ipv4Addr::LOCALHOST.into(),
            Ipv4Addr::new(10, 1, 2, 3).into(),
            Ipv4Addr::new(172, 16, 0, 1).into(),
            Ipv4Addr::new(192, 168, 1, 44).into(),
            Ipv4Addr::new(169, 254, 3, 4).into(),
            Ipv6Addr::LOCALHOST.into(),
        ];
        for ip in lan {
            assert_eq!(classify_source(Some(ip)), KnockSource::Lan, "{ip}");
        }
        let wan: [IpAddr; 6] = [
            Ipv4Addr::new(203, 0, 113, 5).into(),
            Ipv4Addr::new(8, 8, 8, 8).into(),
            // 172.32/12 is outside RFC 1918; 100.128/9 is outside the CGNAT block.
            Ipv4Addr::new(172, 32, 0, 1).into(),
            Ipv4Addr::new(100, 128, 0, 1).into(),
            Ipv4Addr::UNSPECIFIED.into(),
            "2606:4700::1111".parse::<IpAddr>().unwrap(),
        ];
        for ip in wan {
            assert_eq!(classify_source(Some(ip)), KnockSource::Wan, "{ip}");
        }
        assert_eq!(
            classify_source(None),
            KnockSource::Wan,
            "an unknown source fails closed"
        );
        // A v4 address wearing a v6 hat is classified as what it really is.
        assert_eq!(
            classify_source(Some("::ffff:192.168.1.44".parse().unwrap())),
            KnockSource::Lan
        );
        assert_eq!(
            classify_source(Some("::ffff:203.0.113.5".parse().unwrap())),
            KnockSource::Wan
        );
        // fc00::/7 unique-local and fe80::/10 link-local are both on-link.
        assert_eq!(
            classify_source(Some("fd00::1".parse().unwrap())),
            KnockSource::Lan
        );
        assert_eq!(
            classify_source(Some("fe80::1".parse().unwrap())),
            KnockSource::Lan
        );
    }

    /// 100.64/10 is Tailscale's range and a CGNAT ISP's alike, so the route back decides.
    #[test]
    fn a_cgnat_peer_is_lan_only_over_the_tailnet() {
        use super::approval::{classify_with, is_tailscale_iface};
        let peer: IpAddr = "100.96.0.7".parse().unwrap();
        assert_eq!(classify_with(Some(peer), |_| true), KnockSource::Lan);
        // The CGNAT neighbour: the same range, answered out of the ordinary uplink.
        assert_eq!(classify_with(Some(peer), |_| false), KnockSource::Wan);
        let mapped = "::ffff:100.96.0.7".parse().ok();
        assert_eq!(classify_with(mapped, |_| false), KnockSource::Wan);
        // The tailnet is asked about 100.64/10 only.
        for ip in ["203.0.113.5", "100.128.0.1"] {
            assert_eq!(classify_with(ip.parse().ok(), |_| true), KnockSource::Wan);
        }

        for name in ["tailscale0", "Tailscale", "utun4"] {
            assert!(is_tailscale_iface(name, peer), "{name}");
        }
        // An ISP puts its CGNAT address on the uplink.
        for name in ["eth0", "en0", "ppp0", "Ethernet", "wlan0"] {
            assert!(!is_tailscale_iface(name, peer), "{name}");
        }
        // A utun without a 100.64/10 address is some other VPN.
        assert!(!is_tailscale_iface("utun4", "10.8.0.2".parse().unwrap()));
    }

    /// A knock from the internet carries a name it chose for itself, so the one-click approve
    /// cannot admit it. The entry survives the refusal: the operator still has to answer it.
    #[test]
    fn a_wan_knock_is_not_admitted_by_a_bare_approve() {
        let (_temp, p) = temp();
        let np = NativePairing::load_with(Some(p), None, false).unwrap();
        let wan = std::net::IpAddr::V4(std::net::Ipv4Addr::new(203, 0, 113, 5));
        np.note_pending("Friend's Deck", "dd88", Some(wan), None);
        let id = np.pending()[0].id;
        assert_eq!(np.pending()[0].source, KnockSource::Wan);
        assert!(matches!(
            np.approve_pending(id, None, None).unwrap(),
            ApproveOutcome::WanNeedsBoundPin
        ));
        assert!(!np.is_paired("dd88"), "a refused approve pairs nothing");
        assert_eq!(np.pending().len(), 1, "the knock is still waiting");

        // The same knock from the couch is admitted.
        np.note_pending("Living Room", "ee99", Some(LAN_KNOCK), None);
        let lan_id = np
            .pending()
            .iter()
            .find(|x| x.fingerprint == "ee99")
            .unwrap()
            .id;
        assert!(np
            .approve_pending(lan_id, None, None)
            .unwrap()
            .paired()
            .is_some());
        assert!(np.is_paired("ee99"));
    }

    /// An open window is for the device in the operator's hands. It answers the couch, refuses
    /// the internet, and a window named for the WAN device's fingerprint answers it.
    #[test]
    fn an_open_window_does_not_answer_the_internet() {
        let (_temp, p) = temp();
        let np = NativePairing::load_with(Some(p), None, false).unwrap();
        let pin = np.arm(Duration::from_secs(60));
        assert!(
            matches!(np.pin_for_attempt("aa11", KnockSource::Lan), PinAttempt::Pin(x) if x == pin)
        );
        assert!(matches!(
            np.pin_for_attempt("aa11", KnockSource::Wan),
            PinAttempt::UnboundForWan
        ));
        // Refusing must not have consumed the window — the LAN device it was opened for still pairs.
        assert!(
            matches!(np.pin_for_attempt("aa11", KnockSource::Lan), PinAttempt::Pin(x) if x == pin)
        );

        let bound = np.arm_for(Duration::from_secs(60), Some("AA11".into()), None);
        assert!(
            matches!(np.pin_for_attempt("aa11", KnockSource::Wan), PinAttempt::Pin(x) if x == bound),
            "a window named for this fingerprint answers it wherever it knocked from"
        );
        assert!(matches!(
            np.pin_for_attempt("bb22", KnockSource::Wan),
            PinAttempt::BoundToOther
        ));
    }

    /// A window bound to one fingerprint: another peer can neither pair nor burn it
    /// (rejected without a PIN).
    #[test]
    fn armed_pin_is_fingerprint_bindable() {
        let (_temp, p) = temp();
        let np = NativePairing::load_with(Some(p.clone()), None, false).unwrap();
        let pin = np.arm(Duration::from_secs(60));
        assert!(
            matches!(np.pin_for_attempt("aa11", KnockSource::Lan), PinAttempt::Pin(x) if x == pin)
        );
        assert!(matches!(
            np.pin_for_attempt("bb22", KnockSource::Lan),
            PinAttempt::Pin(_)
        ));
        let pin = np.arm_for(Duration::from_secs(60), Some("AA11".into()), None);
        assert!(
            matches!(np.pin_for_attempt("aa11", KnockSource::Lan), PinAttempt::Pin(x) if x == pin)
        );
        assert!(matches!(
            np.pin_for_attempt("bb22", KnockSource::Lan),
            PinAttempt::BoundToOther
        ));
        np.disarm();
        assert!(matches!(
            np.pin_for_attempt("aa11", KnockSource::Lan),
            PinAttempt::Disarmed
        ));
    }

    /// One source IP cannot exceed the per-IP cap. A parked genuine knock is never
    /// evicted by a flood, even one that fills the global cap from many IPs.
    #[test]
    fn pending_per_ip_cap_and_parked_protection() {
        let (_temp, p) = temp();
        let np = NativePairing::load_with(Some(p.clone()), None, false).unwrap();
        let attacker = IpAddr::from([192, 168, 1, 66]);
        for i in 0..20 {
            np.note_pending("flood", &format!("atk{i:03}"), Some(attacker), None);
        }
        assert_eq!(
            np.pending().len(),
            MAX_PENDING_PER_IP,
            "one IP can't exceed the per-IP cap"
        );
        let legit = IpAddr::from([192, 168, 1, 50]);
        let seq = np.note_pending("Living Room", "legit01", Some(legit), None);
        np.set_parked("legit01", seq, true);
        for i in 0..(PENDING_CAP * 2) {
            let ip = IpAddr::from([10, 0, (i / 256) as u8, (i % 256) as u8]);
            np.note_pending("flood2", &format!("g{i:04}"), Some(ip), None);
        }
        assert!(
            np.pending_contains("legit01"),
            "a parked, held-open knock is never evicted by a flood"
        );
        assert!(np.pending().len() <= PENDING_CAP, "global cap still holds");
    }

    fn wall_now() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
    }

    /// A store written before grants existed (name + fingerprint only) decodes as full
    /// control, forever.
    #[test]
    fn pre_grants_store_decodes_as_full_permanent() {
        let (_temp, p) = temp();
        std::fs::write(
            &p,
            br#"{ "clients": [ { "name": "Old Laptop", "fingerprint": "ab12" } ] }"#,
        )
        .unwrap();
        let np = NativePairing::load_with(Some(p.clone()), None, false).unwrap();
        let listed = np.list();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "Old Laptop");
        assert_eq!(listed[0].grants, None, "absent stays absent");
        assert_eq!(listed[0].expires_unix, None);
        assert_eq!(listed[0].granted_unix, None);
        assert!(np.is_paired("AB12"));
        assert_eq!(
            np.effective("AB12", wall_now()),
            Some(GRANT_ALL),
            "absent grants = full control"
        );
    }

    /// Re-running the pairing ceremony must never widen access. `add()` is name-only for
    /// an existing fingerprint.
    #[test]
    fn repair_via_add_never_escalates() {
        use punktfunk_core::quic::GRANT_GAMEPAD;
        let (_temp, p) = temp();
        let np = NativePairing::load_with(Some(p.clone()), None, false).unwrap();
        let now = wall_now();
        let guest = Access {
            grants: GRANT_GAMEPAD,
            expires_unix: Some(now + 3600),
            until_disconnect: false,
        };
        np.add_with_access("Guest Deck", "aa11", Some(guest))
            .unwrap();
        let before = np.list()[0].clone();
        assert_eq!(before.grants, Some(GRANT_GAMEPAD));
        assert_eq!(before.expires_unix, Some(now + 3600));
        assert!(before.granted_unix.is_some());

        np.add("Guest Deck Again", "AA11").unwrap();
        assert_eq!(np.list().len(), 1, "re-pair must not duplicate");
        let after = np.list()[0].clone();
        assert_eq!(after.name, "Guest Deck Again");
        assert_eq!(after.grants, before.grants, "re-pair must NOT touch grants");
        assert_eq!(
            after.expires_unix, before.expires_unix,
            "re-pair must NOT touch expiry"
        );
        assert_eq!(
            after.granted_unix, before.granted_unix,
            "re-pair must NOT re-stamp the grant time"
        );
        assert_eq!(
            np.effective("aa11", now),
            Some(GRANT_GAMEPAD),
            "still controller-only after the re-pair"
        );

        // Limitation is the persisted record, not memory.
        drop(np);
        let np = NativePairing::load_with(Some(p.clone()), None, false).unwrap();
        assert_eq!(np.effective("aa11", now), Some(GRANT_GAMEPAD));
    }

    #[test]
    fn approve_with_access_pins_the_choice() {
        use punktfunk_core::quic::GRANT_GAMEPAD;
        let (_temp, p) = temp();
        let np = NativePairing::load_with(Some(p.clone()), None, false).unwrap();
        let now = wall_now();
        np.note_pending("device bb22", "BB22", Some(LAN_KNOCK), None);
        let id = np.pending()[0].id;
        let client = np
            .approve_pending(
                id,
                Some("Guest Phone"),
                Some(Access {
                    grants: GRANT_GAMEPAD,
                    expires_unix: Some(now + 4 * 3600),
                    until_disconnect: false,
                }),
            )
            .unwrap()
            .paired()
            .unwrap();
        assert_eq!(client.name, "Guest Phone");
        assert_eq!(client.grants, Some(GRANT_GAMEPAD));
        assert_eq!(client.expires_unix, Some(now + 4 * 3600));
        assert!(client.granted_unix.is_some());
        assert_eq!(np.effective("bb22", now), Some(GRANT_GAMEPAD));
    }

    /// Expiry ends authorization, not listing: `effective()` becomes `None` at the
    /// deadline while `is_paired()`/`list()` keep the row.
    #[test]
    fn expiry_flips_effective_but_keeps_the_row() {
        use punktfunk_core::quic::GRANT_GAMEPAD;
        let (_temp, p) = temp();
        let np = NativePairing::load_with(Some(p.clone()), None, false).unwrap();
        let now = wall_now();
        np.add_with_access(
            "Evening Guest",
            "cc33",
            Some(Access {
                grants: GRANT_GAMEPAD,
                expires_unix: Some(now + 10),
                until_disconnect: false,
            }),
        )
        .unwrap();
        assert_eq!(np.effective("cc33", now), Some(GRANT_GAMEPAD));
        assert_eq!(
            np.effective("cc33", now + 9),
            Some(GRANT_GAMEPAD),
            "still authorized one second before the deadline"
        );
        assert_eq!(
            np.effective("cc33", now + 10),
            None,
            "the deadline itself expires"
        );
        assert_eq!(np.effective("cc33", now + 3600), None);
        assert!(np.is_paired("cc33"), "expired but still LISTED");
        assert_eq!(np.list().len(), 1, "the row survives for the console");
    }

    /// A store that smuggles reserved bits cannot feed them into enforcement:
    /// `effective()` and the watch mask with GRANT_ALL on read.
    #[test]
    fn reserved_bits_are_masked_on_read() {
        let (_temp, p) = temp();
        let np = NativePairing::load_with(Some(p.clone()), None, false).unwrap();
        np.add("Future Device", "dd44").unwrap();
        let from_the_future = Access {
            grants: GRANT_ALL | (1 << 30),
            expires_unix: None,
            until_disconnect: false,
        };
        assert!(np.set_access("dd44", from_the_future).unwrap());
        assert_eq!(
            np.effective("dd44", wall_now()),
            Some(GRANT_ALL),
            "reserved bit masked off on read"
        );
        assert_eq!(np.subscribe("dd44").borrow().grants, GRANT_ALL);
        assert!(!np.set_access("nope99", from_the_future).unwrap());
        assert!(!np.is_paired("nope99"));
    }

    /// An edit reaches a live subscriber in one event; unpair publishes `revoked`.
    /// Re-pairing publishes fresh state on the same channel.
    #[tokio::test]
    async fn watch_publishes_on_set_access_and_unpair() {
        use punktfunk_core::quic::GRANT_GAMEPAD;
        let (_temp, p) = temp();
        let np = NativePairing::load_with(Some(p.clone()), None, false).unwrap();
        np.add("Living Room", "ee55").unwrap();
        // Registry keys case-insensitively, like the store.
        let mut rx = np.subscribe("EE55");
        assert_eq!(
            *rx.borrow(),
            AccessState {
                grants: GRANT_ALL,
                deadline_unix: None,
                revoked: false
            },
            "initial value is the state now"
        );

        let now = wall_now();
        np.set_access(
            "ee55",
            Access {
                grants: GRANT_GAMEPAD,
                expires_unix: Some(now + 60),
                until_disconnect: false,
            },
        )
        .unwrap();
        rx.changed().await.unwrap();
        assert_eq!(
            *rx.borrow(),
            AccessState {
                grants: GRANT_GAMEPAD,
                deadline_unix: Some(now + 60),
                revoked: false
            }
        );

        assert!(np.remove("ee55").unwrap());
        rx.changed().await.unwrap();
        assert_eq!(
            *rx.borrow(),
            AccessState {
                grants: 0,
                deadline_unix: None,
                revoked: true
            }
        );

        // Same channel: a stale-but-alive subscriber sees the new state.
        np.add("Living Room", "ee55").unwrap();
        rx.changed().await.unwrap();
        assert!(!rx.borrow().revoked);
        assert_eq!(rx.borrow().grants, GRANT_ALL);

        assert!(np.subscribe("zz99").borrow().revoked);
    }

    /// Absent Moonlight record = ungoverned full control (the GameStream cert list is
    /// pairing authority). A record that exists governs as native, including expiry
    /// failing closed. Flipping the absent arm to `None` would revoke every ungoverned
    /// Moonlight pairing on upgrade.
    #[test]
    fn moonlight_effective_absent_is_ungoverned_but_a_record_governs() {
        use punktfunk_core::quic::GRANT_GAMEPAD;
        let (_temp, p) = temp();
        let np = NativePairing::load_with(Some(p.clone()), None, false).unwrap();
        let now = wall_now();

        assert_eq!(np.effective("ab12", now), None, "native: unpaired");
        assert_eq!(
            np.moonlight_effective("ab12", now),
            Some(GRANT_ALL),
            "moonlight: ungoverned = full control"
        );

        np.add_with_access(
            "Guest Deck",
            "AB12",
            Some(Access {
                grants: GRANT_GAMEPAD,
                expires_unix: Some(now + 60),
                until_disconnect: false,
            }),
        )
        .unwrap();
        assert_eq!(np.moonlight_effective("ab12", now), Some(GRANT_GAMEPAD));
        assert_eq!(
            np.moonlight_effective("ab12", now),
            np.effective("ab12", now)
        );

        assert_eq!(np.moonlight_effective("ab12", now + 60), None);
        assert_eq!(
            np.moonlight_effective("ab12", now + 60),
            np.effective("ab12", now + 60)
        );

        // Unpair here returns ungoverned; GameStream pairing is a separate store.
        assert!(np.remove("ab12").unwrap());
        assert_eq!(np.moonlight_effective("ab12", now), Some(GRANT_ALL));
    }

    #[test]
    fn armed_window_carries_access_until_consumed() {
        use punktfunk_core::quic::GRANT_GAMEPAD;
        let (_temp, p) = temp();
        let np = NativePairing::load_with(Some(p.clone()), None, false).unwrap();
        assert_eq!(np.armed_access(), None, "disarmed = no choice");
        let choice = Access {
            grants: GRANT_GAMEPAD,
            expires_unix: Some(wall_now() + 3600),
            until_disconnect: false,
        };
        np.arm_for(Duration::from_secs(60), None, Some(choice));
        assert_eq!(np.armed_access(), Some(choice));
        np.disarm();
        assert_eq!(np.armed_access(), None, "consumed with the window");
    }
}
