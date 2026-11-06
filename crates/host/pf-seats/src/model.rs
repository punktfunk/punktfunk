//! Persisted seat model and deterministic resource allocation.
//!
//! A ledger has one schema version and at most four seats. IDs remain stable
//! while names, accounts, ports, and display slots are unique. Display slots
//! are fixed at 12 through 15; port pools are explicit so a later platform
//! backend can choose a different reserved range without changing the ledger.
//! Every value is validated again after deserialization. Runtime state is an
//! observation, not a substitute for the backend's own process identity.
//!
//! On Linux one row may be the box owner's: an account that already exists,
//! adopted rather than created. It takes a slot and ports like any seat. Any other
//! Linux row may adopt a person's own account too, recorded by its uid.
//! On Windows a row may adopt a person's own account, named by its SID; its
//! account follows the SAM naming rule instead of the one for made accounts.

use rand::Rng;
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::HashSet;
use std::fmt;
use std::str::FromStr;

pub const LEDGER_SCHEMA_VERSION: u32 = 1;
pub const MAX_SEATS: usize = 4;
pub const DISPLAY_SLOTS: [u8; MAX_SEATS] = [12, 13, 14, 15];
/// The console host's own defaults, which a test here holds equal to
/// `pf_host_config::listen`. A seat on either could never run beside the console host.
pub const RESERVED_PORTS: [u16; 2] = [CONSOLE_NATIVE_PORT, CONSOLE_MGMT_PORT];
pub const CONSOLE_NATIVE_PORT: u16 = 9777;
pub const CONSOLE_MGMT_PORT: u16 = 47990;
/// After the browser plane's 9778.
pub const DEFAULT_NATIVE_PORTS: [u16; MAX_SEATS] = [9779, 9780, 9781, 9782];
pub const DEFAULT_MGMT_PORTS: [u16; MAX_SEATS] = [47995, 47996, 47997, 47998];
/// Each seat's browser plane, beside its native port: the seat whose native port is the n-th of
/// [`DEFAULT_NATIVE_PORTS`] streams to browsers on the n-th of these.
pub const DEFAULT_BROWSER_PORTS: [u16; MAX_SEATS] = [9783, 9784, 9785, 9786];
/// `LM20_UNLEN`: NetUserAdd rejects longer SAM account names.
pub const MAX_ACCOUNT_LEN: usize = 20;
/// Bytes of backend error text a seat's runtime status keeps.
pub const MAX_RUNTIME_DETAIL: usize = 512;

/// Whether `uid` can be a person's own Linux account: 1000 up, outside systemd's 60001–65535
/// (homed users, dynamic service users, `nobody`) and below 2^31, where signed code reads it as
/// negative. FreeIPA and sssd's AD mapping allocate above 65535.
pub fn ordinary_uid(uid: u32) -> bool {
    (1000..0x8000_0000).contains(&uid) && !(60001..=65535).contains(&uid)
}

/// The longest Linux owner account. The owner's row takes the account as its name too, and a
/// row's name stops at 64.
pub const MAX_OWNER_ACCOUNT_LEN: usize = 64;

/// Whether `value` can name the Linux owner's own account: lowercase letters, digits, `_`, `-`,
/// `.` and `@`, starting with a letter or `_`, at most [`MAX_OWNER_ACCOUNT_LEN`] long. `.` and
/// `@` admit a directory service's `user@domain`. The name lands in unit instances, paths and
/// argv, so a `/`, a `\`, whitespace or a leading `-` never passes.
pub fn valid_owner_account(value: &str) -> bool {
    let mut bytes = value.bytes();
    value.len() <= MAX_OWNER_ACCOUNT_LEN
        && bytes
            .next()
            .is_some_and(|b| b.is_ascii_lowercase() || b == b'_')
        && bytes.all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-' | b'.' | b'@')
        })
}

#[derive(Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct SeatId(String);

impl<'de> Deserialize<'de> for SeatId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Self::parse(value).map_err(serde::de::Error::custom)
    }
}

impl SeatId {
    pub fn parse(value: impl Into<String>) -> Result<Self, ValidationError> {
        let value = value.into();
        if value.len() != 32
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(ValidationError::InvalidId);
        }
        Ok(Self(value))
    }

    pub fn random() -> Self {
        let mut bytes = [0_u8; 16];
        rand::rng().fill_bytes(&mut bytes);
        Self(hex::encode(bytes))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SeatId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_tuple("SeatId").field(&self.0).finish()
    }
}

impl fmt::Display for SeatId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl FromStr for SeatId {
    type Err = ValidationError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeState {
    Starting,
    Running,
    Stopping,
    Stopped,
    Failed,
    #[default]
    Unknown,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeStatus {
    pub state: RuntimeState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl RuntimeStatus {
    pub fn stopped() -> Self {
        Self {
            state: RuntimeState::Stopped,
            detail: None,
        }
    }

    pub fn running() -> Self {
        Self {
            state: RuntimeState::Running,
            detail: None,
        }
    }

    /// Backend error text is unbounded; capping it here keeps one long
    /// message from failing every later ledger save.
    pub fn failed(detail: impl Into<String>) -> Self {
        let mut detail = detail.into();
        if detail.len() > MAX_RUNTIME_DETAIL {
            let mut end = MAX_RUNTIME_DETAIL;
            while !detail.is_char_boundary(end) {
                end -= 1;
            }
            detail.truncate(end);
        }
        Self {
            state: RuntimeState::Failed,
            detail: Some(detail),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Seat {
    pub id: SeatId,
    pub name: String,
    pub account: String,
    pub display_slot: u8,
    pub native_port: u16,
    pub mgmt_port: u16,
    #[serde(default)]
    pub autostart: bool,
    #[serde(default)]
    pub runtime: RuntimeStatus,
    /// The box owner's row (Linux). Its account is never created or deleted by the ledger.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub owner: bool,
    /// SHA-256 of the seat host's certificate DER, lowercase hex: the pin a client dials it
    /// with, carried by the box's `Redirect`. Learned while the seat runs; `None` before its
    /// host has minted an identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    /// A person's own account this row adopted, by SID. `None`: an account the ledger made and
    /// marks by comment. Never created or deleted; released with its rights put back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adopted_sid: Option<String>,
    /// A person's own Linux account this row adopted, by the uid the daemon resolved at adopt.
    /// A name that later resolves to another uid is not the row's. Never created or deleted.
    /// `None`: a made account, the owner's row or a Windows one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adopted_uid: Option<u32>,
    /// Adopt put the account in Remote Desktop Users; release takes it out again.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub rdp_granted: bool,
}

impl Seat {
    /// Whether the row runs on a person's own account, adopted by SID or by uid. The box
    /// owner's row is not one: it is the owner's by role.
    pub fn adopted(&self) -> bool {
        self.adopted_sid.is_some() || self.adopted_uid.is_some()
    }

    /// The UDP port this seat's browser plane listens on ([`browser_port_for`]).
    pub fn browser_port(&self) -> Option<u16> {
        browser_port_for(self.native_port)
    }
}

/// The browser port beside `native_port`: its place in the pool. `None` for a native port from
/// outside it, which no seat is given.
pub fn browser_port_for(native_port: u16) -> Option<u16> {
    let at = DEFAULT_NATIVE_PORTS
        .iter()
        .position(|&p| p == native_port)?;
    Some(DEFAULT_BROWSER_PORTS[at])
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ledger {
    pub schema_version: u32,
    #[serde(default)]
    pub generation: u64,
    #[serde(default)]
    pub seats: Vec<Seat>,
}

impl Default for Ledger {
    fn default() -> Self {
        Self::new()
    }
}

impl Ledger {
    pub fn new() -> Self {
        Self {
            schema_version: LEDGER_SCHEMA_VERSION,
            generation: 0,
            seats: Vec::new(),
        }
    }

    pub fn validate(&self) -> Result<(), ValidationError> {
        if self.schema_version != LEDGER_SCHEMA_VERSION {
            return Err(ValidationError::UnsupportedSchema(self.schema_version));
        }
        if self.seats.len() > MAX_SEATS {
            return Err(ValidationError::TooManySeats(self.seats.len()));
        }

        let mut ids = HashSet::new();
        let mut names = HashSet::new();
        let mut accounts = HashSet::new();
        let mut sids = HashSet::new();
        let mut uids = HashSet::new();
        let mut slots = HashSet::new();
        let mut ports = HashSet::new();
        if self.seats.iter().filter(|seat| seat.owner).count() > 1 {
            return Err(ValidationError::Duplicate {
                kind: "owner",
                value: "a second owner row".into(),
            });
        }
        for seat in &self.seats {
            SeatId::parse(seat.id.as_str())?;
            validate_name(&seat.name)?;
            validate_identity(
                &seat.account,
                seat.owner,
                seat.adopted_sid.as_deref(),
                seat.adopted_uid,
            )?;
            if !DISPLAY_SLOTS.contains(&seat.display_slot) {
                return Err(ValidationError::InvalidDisplaySlot(seat.display_slot));
            }
            if seat.native_port == 0 || RESERVED_PORTS.contains(&seat.native_port) {
                return Err(ValidationError::InvalidPort("native", seat.native_port));
            }
            if seat.mgmt_port == 0 || RESERVED_PORTS.contains(&seat.mgmt_port) {
                return Err(ValidationError::InvalidPort("mgmt", seat.mgmt_port));
            }
            if seat
                .runtime
                .detail
                .as_ref()
                .is_some_and(|v| v.len() > MAX_RUNTIME_DETAIL)
            {
                return Err(ValidationError::RuntimeDetailTooLong);
            }

            unique(&mut ids, seat.id.as_str().to_owned(), "id")?;
            // The owner's row is named for their login, which no host ever announces.
            if !seat.owner {
                unique(&mut names, seat.name.to_lowercase(), "name")?;
            }
            unique(&mut accounts, seat.account.to_lowercase(), "account")?;
            // A renamed account keeps its SID, so a second row for it differs only there.
            if let Some(sid) = &seat.adopted_sid {
                unique(&mut sids, sid.clone(), "account SID")?;
            }
            // Two names can resolve to one user: `jdoe` and `jdoe@corp.example.com`.
            if let Some(uid) = seat.adopted_uid {
                unique(&mut uids, uid, "account uid")?;
            }
            unique(&mut slots, seat.display_slot, "display slot")?;
            unique(&mut ports, seat.native_port, "port")?;
            unique(&mut ports, seat.mgmt_port, "port")?;
        }
        Ok(())
    }

    pub fn allocate(&mut self, request: CreateSeat) -> Result<Seat, ValidationError> {
        let id = loop {
            let id = SeatId::random();
            if !self.seats.iter().any(|seat| seat.id == id) {
                break id;
            }
        };
        self.allocate_with_id(request, id, false, None, None)
    }

    /// The row for a person's own account `request.account`, whose SID is `sid`, with the slot
    /// and ports a seat gets. The caller checks that the SID names that account.
    pub fn allocate_adopted(
        &mut self,
        request: CreateSeat,
        sid: String,
    ) -> Result<Seat, ValidationError> {
        self.allocate_with_id(request, SeatId::random(), false, Some(sid), None)
    }

    /// The row for a person's own Linux account `request.account`, which the daemon resolved to
    /// `uid`, with the slot and ports a seat gets.
    pub fn allocate_adopted_user(
        &mut self,
        request: CreateSeat,
        uid: u32,
    ) -> Result<Seat, ValidationError> {
        self.allocate_with_id(request, SeatId::random(), false, None, Some(uid))
    }

    /// The owner's row for `account`, with the slot and ports a seat gets. The caller checks
    /// that the account exists and that no owner row does.
    pub fn allocate_owner(&mut self, account: &str) -> Result<Seat, ValidationError> {
        let request = CreateSeat {
            name: account.to_owned(),
            account: account.to_owned(),
            autostart: false,
        };
        self.allocate_with_id(request, SeatId::random(), true, None, None)
    }

    /// Adds the row `request` asks for under `id`; `adopted_sid` or `adopted_uid` marks a
    /// person's own account.
    fn allocate_with_id(
        &mut self,
        request: CreateSeat,
        id: SeatId,
        owner: bool,
        adopted_sid: Option<String>,
        adopted_uid: Option<u32>,
    ) -> Result<Seat, ValidationError> {
        self.validate()?;
        if self.seats.len() == MAX_SEATS {
            return Err(ValidationError::TooManySeats(MAX_SEATS + 1));
        }

        let request = request.normalized();
        validate_name(&request.name)?;
        validate_identity(&request.account, owner, adopted_sid.as_deref(), adopted_uid)?;
        let used_slots: HashSet<_> = self.seats.iter().map(|seat| seat.display_slot).collect();
        let used_ports: HashSet<_> = self
            .seats
            .iter()
            .flat_map(|seat| [seat.native_port, seat.mgmt_port])
            .collect();
        let display_slot = first_free(&DISPLAY_SLOTS, &used_slots, "display slot")?;
        let native_port = first_free(&DEFAULT_NATIVE_PORTS, &used_ports, "native port")?;
        let mut ports_with_native = used_ports;
        ports_with_native.insert(native_port);
        let mgmt_port = first_free(&DEFAULT_MGMT_PORTS, &ports_with_native, "mgmt port")?;
        let seat = Seat {
            id,
            name: request.name,
            account: request.account,
            display_slot,
            native_port,
            mgmt_port,
            autostart: request.autostart,
            runtime: RuntimeStatus::stopped(),
            owner,
            fingerprint: None,
            adopted_sid,
            adopted_uid,
            rdp_granted: false,
        };
        self.insert(seat.clone())?;
        Ok(seat)
    }

    /// Adds `seat`, which [`Self::allocate`] returned for a copy of this ledger, if it still fits.
    pub fn insert(&mut self, seat: Seat) -> Result<(), ValidationError> {
        self.seats.push(seat);
        let valid = self.validate();
        if valid.is_err() {
            self.seats.pop();
        }
        valid
    }

    pub fn seat(&self, id: &SeatId) -> Option<&Seat> {
        self.seats.iter().find(|seat| &seat.id == id)
    }

    pub fn owner(&self) -> Option<&Seat> {
        self.seats.iter().find(|seat| seat.owner)
    }

    pub fn remove(&mut self, id: &SeatId) -> Option<Seat> {
        let index = self.seats.iter().position(|seat| &seat.id == id)?;
        Some(self.seats.remove(index))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateSeat {
    pub name: String,
    pub account: String,
    #[serde(default)]
    pub autostart: bool,
}

impl CreateSeat {
    fn normalized(mut self) -> Self {
        self.name = self.name.trim().to_owned();
        self.account = self.account.trim().to_owned();
        self
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ValidationError {
    #[error("ledger schema {0} is unsupported")]
    UnsupportedSchema(u32),
    #[error("a ledger may contain at most four seats, got {0}")]
    TooManySeats(usize),
    #[error("seat id must be 32 lowercase hexadecimal characters")]
    InvalidId,
    #[error("seat name must be 1..=64 characters without surrounding whitespace or controls")]
    InvalidName,
    #[error("account must be 1..=20 ASCII letters, digits, '.', '_', or '-'")]
    InvalidAccount,
    #[error("owner account must be 1..=64 lowercase letters, digits, '_', '-', '.' or '@'")]
    InvalidOwnerAccount,
    #[error("adopted account must be a Windows account name of 1..=20 characters")]
    InvalidSamAccount,
    #[error("adopted account SID must be S-1-5-21-<n>-<n>-<n>-<rid> with a RID of 1000 or more")]
    InvalidSid,
    #[error("an adopted Linux account is an ordinary user's uid, on a row that isn't the owner's")]
    InvalidUid,
    #[error("display slot {0} is outside the reserved 12..=15 range")]
    InvalidDisplaySlot(u8),
    #[error("{0} port {1} is zero or reserved for the console host")]
    InvalidPort(&'static str, u16),
    #[error("duplicate {kind} '{value}'")]
    Duplicate { kind: &'static str, value: String },
    #[error("runtime status detail exceeds 512 bytes")]
    RuntimeDetailTooLong,
    #[error("no free {0} remains")]
    NoAllocation(&'static str),
}

fn validate_name(value: &str) -> Result<(), ValidationError> {
    let len = value.chars().count();
    if value.trim() != value || !(1..=64).contains(&len) || value.chars().any(char::is_control) {
        return Err(ValidationError::InvalidName);
    }
    Ok(())
}

fn validate_account(value: &str) -> Result<(), ValidationError> {
    if value.trim() != value
        || value.is_empty()
        || value.len() > MAX_ACCOUNT_LEN
        || matches!(value, "." | "..")
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(ValidationError::InvalidAccount);
    }
    Ok(())
}

/// A made row's account follows [`validate_account`]. The Linux owner's and a Linux person's
/// follow [`valid_owner_account`], the latter with an ordinary uid. A Windows adopted row's
/// follows the SAM rule and carries a local user's SID. No row is adopted by both SID and uid.
fn validate_identity(
    account: &str,
    owner: bool,
    adopted_sid: Option<&str>,
    adopted_uid: Option<u32>,
) -> Result<(), ValidationError> {
    let linux = || {
        valid_owner_account(account)
            .then_some(())
            .ok_or(ValidationError::InvalidOwnerAccount)
    };
    match (adopted_sid, adopted_uid) {
        (None, None) if owner => linux(),
        (None, None) => validate_account(account),
        (Some(sid), None) => {
            validate_sam_account(account)?;
            validate_sid(sid)
        }
        (None, Some(uid)) if !owner && ordinary_uid(uid) => linux(),
        (_, Some(_)) => Err(ValidationError::InvalidUid),
    }
}

/// A Windows SAM account name: 1..=20 characters, no controls, no surrounding whitespace, none of
/// `"/\[]:;|=,+*?<>`, and not only dots and spaces. Spaces and non-ASCII letters pass.
pub(crate) fn validate_sam_account(value: &str) -> Result<(), ValidationError> {
    if value.trim() != value
        || !(1..=MAX_ACCOUNT_LEN).contains(&value.chars().count())
        || value
            .chars()
            .any(|c| c.is_control() || r#""/\[]:;|=,+*?<>"#.contains(c))
        || value.chars().all(|c| matches!(c, '.' | ' '))
    {
        return Err(ValidationError::InvalidSamAccount);
    }
    Ok(())
}

/// A local user's SID, `S-1-5-21-<n>-<n>-<n>-<rid>`. RIDs below 1000 are built-ins such as the
/// Administrator (500), which no row adopts.
fn validate_sid(value: &str) -> Result<(), ValidationError> {
    let parts: Vec<Option<u32>> = value
        .strip_prefix("S-1-5-21-")
        .ok_or(ValidationError::InvalidSid)?
        .split('-')
        .map(|n| {
            if n.bytes().all(|b| b.is_ascii_digit()) {
                n.parse().ok()
            } else {
                None
            }
        })
        .collect();
    match parts[..] {
        [Some(_), Some(_), Some(_), Some(rid)] if rid >= 1000 => Ok(()),
        _ => Err(ValidationError::InvalidSid),
    }
}

fn unique<T>(set: &mut HashSet<T>, value: T, kind: &'static str) -> Result<(), ValidationError>
where
    T: Eq + std::hash::Hash + fmt::Display,
{
    let rendered = value.to_string();
    if set.insert(value) {
        Ok(())
    } else {
        Err(ValidationError::Duplicate {
            kind,
            value: rendered,
        })
    }
}

fn first_free<T: Copy + Eq + std::hash::Hash>(
    candidates: &[T],
    used: &HashSet<T>,
    kind: &'static str,
) -> Result<T, ValidationError> {
    candidates
        .iter()
        .copied()
        .find(|candidate| !used.contains(candidate))
        .ok_or(ValidationError::NoAllocation(kind))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(n: usize) -> CreateSeat {
        CreateSeat {
            name: format!("Player {n}"),
            account: format!("pf-seat-{n}"),
            autostart: n.is_multiple_of(2),
        }
    }

    #[test]
    fn allocation_uses_the_reserved_resources_and_reuses_only_freed_ones() {
        let mut ledger = Ledger::new();
        let first_id = SeatId::parse("01".repeat(16)).unwrap();
        let first = ledger
            .allocate_with_id(request(1), first_id.clone(), false, None, None)
            .unwrap();
        let second = ledger.allocate(request(2)).unwrap();
        assert_eq!(
            (first.display_slot, first.native_port, first.mgmt_port),
            (12, 9779, 47995)
        );
        assert_eq!(
            (second.display_slot, second.native_port, second.mgmt_port),
            (13, 9780, 47996)
        );
        assert_eq!(ledger.seat(&first_id).unwrap().id, first_id);
        ledger.remove(&first_id).unwrap();
        let replacement = ledger.allocate(request(3)).unwrap();
        assert_eq!(replacement.display_slot, 12);
        assert_ne!(replacement.id, first_id);
    }

    /// A seat allocated on a copy lands in the ledger it was copied from, unless the copy's view
    /// has gone stale.
    #[test]
    fn a_seat_allocated_on_a_copy_is_inserted_into_the_original() {
        let mut ledger = Ledger::new();
        let seat = ledger.clone().allocate(request(1)).unwrap();
        ledger.insert(seat.clone()).unwrap();
        assert_eq!(ledger.seats, std::slice::from_ref(&seat));
        assert!(
            ledger.insert(seat).is_err(),
            "the same seat twice collides on id"
        );
        assert_eq!(ledger.seats.len(), 1);
    }

    #[test]
    fn a_long_backend_error_still_persists() {
        let status = RuntimeStatus::failed("€".repeat(300));
        assert!(status.detail.as_ref().unwrap().len() <= MAX_RUNTIME_DETAIL);
        let mut ledger = Ledger::new();
        let mut seat = ledger.allocate(request(1)).unwrap();
        seat.runtime = status;
        ledger.seats = vec![seat];
        assert!(ledger.validate().is_ok());
    }

    /// A seat's browser port follows its native port, and no pool shares a port with another or
    /// with the console host's own.
    #[test]
    fn a_seat_s_browser_port_sits_beside_its_native_port() {
        let mut ledger = Ledger::new();
        let first = ledger.allocate(request(1)).unwrap();
        let second = ledger.allocate(request(2)).unwrap();
        assert_eq!(first.browser_port(), Some(9783));
        assert_eq!(second.browser_port(), Some(9784));
        let off_pool = Seat {
            native_port: 19779,
            ..first
        };
        assert_eq!(off_pool.browser_port(), None);
        let mut all: Vec<u16> = DEFAULT_NATIVE_PORTS
            .iter()
            .chain(&DEFAULT_MGMT_PORTS)
            .chain(&DEFAULT_BROWSER_PORTS)
            .chain(&RESERVED_PORTS)
            .chain(&[pf_host_config::listen::BROWSER_PORT])
            .copied()
            .collect();
        let count = all.len();
        all.sort_unstable();
        all.dedup();
        assert_eq!(all.len(), count, "a port in two pools");
    }

    #[test]
    fn console_host_and_control_ports_are_never_seat_ports() {
        // This crate links the host's config in tests only, so both spell the two ports.
        use pf_host_config::listen::{MGMT_PORT, NATIVE_PORT};
        assert_eq!(RESERVED_PORTS, [NATIVE_PORT, MGMT_PORT]);
        let mut ledger = Ledger::new();
        let mut seat = ledger.allocate(request(1)).unwrap();
        for port in RESERVED_PORTS {
            seat.native_port = port;
            ledger.seats = vec![seat.clone()];
            assert!(matches!(
                ledger.validate(),
                Err(ValidationError::InvalidPort("native", found)) if found == port
            ));
        }
        for port in RESERVED_PORTS {
            seat.native_port = DEFAULT_NATIVE_PORTS[0];
            seat.mgmt_port = port;
            ledger.seats = vec![seat.clone()];
            assert!(matches!(
                ledger.validate(),
                Err(ValidationError::InvalidPort("mgmt", found)) if found == port
            ));
        }
    }

    #[test]
    fn validation_rejects_every_cross_seat_collision() {
        let mut ledger = Ledger::new();
        let mut second = ledger.allocate(request(1)).unwrap();
        let first = second.clone();
        second.id = SeatId::parse("ab".repeat(16)).unwrap();
        second.name = "Other".into();
        second.account = "other".into();
        second.display_slot = 13;
        second.native_port = 9780;
        second.mgmt_port = 47996;
        ledger.seats = vec![first, second];
        assert!(ledger.validate().is_ok());

        for collision in [
            "id",
            "name",
            "account",
            "slot",
            "native",
            "mgmt",
            "cross_port",
        ] {
            let mut invalid = ledger.clone();
            match collision {
                "id" => invalid.seats[1].id = invalid.seats[0].id.clone(),
                "name" => invalid.seats[1].name = invalid.seats[0].name.to_uppercase(),
                "account" => invalid.seats[1].account = invalid.seats[0].account.to_uppercase(),
                "slot" => invalid.seats[1].display_slot = invalid.seats[0].display_slot,
                "native" => invalid.seats[1].native_port = invalid.seats[0].native_port,
                "mgmt" => invalid.seats[1].mgmt_port = invalid.seats[0].mgmt_port,
                "cross_port" => invalid.seats[1].native_port = invalid.seats[0].mgmt_port,
                _ => unreachable!(),
            }
            assert!(invalid.validate().is_err(), "{collision} collision passed");
        }
    }

    #[test]
    fn allocation_stops_at_four_seats() {
        let mut ledger = Ledger::new();
        for n in 0..MAX_SEATS {
            ledger.allocate(request(n)).unwrap();
        }
        assert!(matches!(
            ledger.allocate(request(MAX_SEATS)),
            Err(ValidationError::TooManySeats(5))
        ));
    }

    /// The owner's row takes a slot and ports from the pools a seat uses, counts toward the four,
    /// and is the only one. A row written before the field existed reads as not the owner.
    #[test]
    fn the_owner_row_shares_the_allocator_and_is_unique() {
        let mut ledger = Ledger::new();
        let first = ledger.allocate(request(1)).unwrap();
        let owner = ledger.allocate_owner("enrico").unwrap();
        assert!(owner.owner && !first.owner);
        assert_eq!(
            (owner.display_slot, owner.native_port, owner.mgmt_port),
            (13, 9780, 47996)
        );
        assert_eq!(owner.name, "enrico");
        assert_eq!(ledger.owner().map(|s| &s.id), Some(&owner.id));

        let mut second = ledger.clone();
        second.seats[0].owner = true;
        assert!(matches!(
            second.validate(),
            Err(ValidationError::Duplicate { kind: "owner", .. })
        ));

        let wire = serde_json::to_value(&first).unwrap();
        assert!(
            wire.get("owner").is_none(),
            "a seat's wire form is unchanged"
        );
        assert_eq!(serde_json::to_value(&owner).unwrap()["owner"], true);
        let old: Seat = serde_json::from_value(wire).unwrap();
        assert!(!old.owner);
    }

    /// The owner may be a directory service's `user@domain`. Nothing that could leave a unit
    /// instance, a path or an argument passes.
    #[test]
    fn an_owner_account_may_be_qualified() {
        let longest = "a".repeat(MAX_OWNER_ACCOUNT_LEN);
        for good in [
            "enrico",
            "_svc",
            "a-b_c",
            "j.doe",
            "jdoe@corp.example.com",
            &longest,
        ] {
            assert!(valid_owner_account(good), "{good}");
        }
        let too_long = "a".repeat(MAX_OWNER_ACCOUNT_LEN + 1);
        for bad in [
            "",
            "Enrico",
            "1abc",
            "-s",
            ".x",
            "@corp",
            "a b",
            "a/b",
            r"corp\jdoe",
            "a\u{e9}",
            &too_long,
        ] {
            assert!(!valid_owner_account(bad), "{bad:?}");
        }
        let mut ledger = Ledger::new();
        let owner = ledger.allocate_owner("jdoe@corp.example.com").unwrap();
        assert_eq!(owner.name, "jdoe@corp.example.com");
        assert!(ledger.validate().is_ok());
        assert!(matches!(
            Ledger::new().allocate_owner("Enrico"),
            Err(ValidationError::InvalidOwnerAccount)
        ));
        let made = CreateSeat {
            name: "jdoe".into(),
            account: "jdoe@corp".into(),
            autostart: false,
        };
        assert!(
            matches!(
                Ledger::new().allocate(made),
                Err(ValidationError::InvalidAccount)
            ),
            "a made row keeps the portable rule"
        );
    }

    /// An adopted row takes a slot and ports from the pools a seat uses, names its account by the
    /// SAM rule and carries a local user's SID; a made row keeps the portable rule. Accounts stay
    /// unique across both kinds. A row written before the fields existed reads as made.
    #[test]
    fn an_adopted_row_shares_the_allocator_and_follows_the_sam_rule() {
        const SID: &str = "S-1-5-21-3623811015-3361044348-30300820-1001";
        let named = |name: &str, account: &str| CreateSeat {
            name: name.into(),
            account: account.into(),
            autostart: false,
        };
        let mut ledger = Ledger::new();
        let first = ledger.allocate(request(1)).unwrap();
        let adopted = ledger
            .allocate_adopted(named("Bärbel", "Bärbel Ott"), SID.into())
            .unwrap();
        assert_eq!(
            (adopted.display_slot, adopted.native_port, adopted.mgmt_port),
            (13, 9780, 47996)
        );
        assert_eq!(adopted.adopted_sid.as_deref(), Some(SID));
        assert!(!adopted.rdp_granted && !adopted.owner);
        assert!(ledger.validate().is_ok());

        assert!(matches!(
            Ledger::new().allocate(named("Bärbel", "Bärbel Ott")),
            Err(ValidationError::InvalidAccount)
        ));
        let twenty = "ü".repeat(20);
        assert!(validate_sam_account(&twenty).is_ok(), "counted as chars");
        let too_long = "ü".repeat(21);
        for bad in [
            "", " Ben", "Ben ", "a/b", r"a\b", "Ben?", "a<b", "...", ". .", "a\tb", &too_long,
        ] {
            assert!(validate_sam_account(bad).is_err(), "{bad:?}");
        }
        assert!(matches!(
            Ledger::new().allocate_adopted(named("Bärbel", "a:b"), SID.into()),
            Err(ValidationError::InvalidSamAccount)
        ));
        for bad in [
            "",
            "S-1-5-21-1-2-3-500",
            "S-1-5-32-544",
            "S-1-5-21-1-2-1001",
            "S-1-5-21-1-2-3-4-1001",
            "S-1-5-21-1-2-+3-1001",
            "S-1-5-21-1-2--1001",
            "S-1-5-21-1-2-3-99999999999",
        ] {
            assert!(
                matches!(
                    Ledger::new().allocate_adopted(named("Bärbel", "Bärbel Ott"), bad.into()),
                    Err(ValidationError::InvalidSid)
                ),
                "{bad:?}"
            );
        }

        let other_sid = "S-1-5-21-1-2-3-1002".to_string();
        assert!(matches!(
            ledger
                .clone()
                .allocate_adopted(named("Kid", "PF-SEAT-1"), other_sid),
            Err(ValidationError::Duplicate {
                kind: "account",
                ..
            })
        ));
        assert!(matches!(
            ledger
                .clone()
                .allocate_adopted(named("Kid", "Barbel"), SID.into()),
            Err(ValidationError::Duplicate {
                kind: "account SID",
                ..
            })
        ));

        let wire = serde_json::to_value(&first).unwrap();
        assert!(
            wire.get("adopted_sid").is_none() && wire.get("rdp_granted").is_none(),
            "a made seat's wire form is unchanged"
        );
        let old: Seat = serde_json::from_value(wire).unwrap();
        assert_eq!(old, first);
        let granted = Seat {
            rdp_granted: true,
            ..adopted
        };
        let wire = serde_json::to_value(&granted).unwrap();
        assert_eq!(
            (&wire["adopted_sid"], &wire["rdp_granted"]),
            (&SID.into(), &true.into())
        );
        assert_eq!(serde_json::from_value::<Seat>(wire).unwrap(), granted);
    }

    /// A Linux person's row records the uid the daemon resolved, names its account by the owner
    /// rule, and never shares that uid with another row. A ledger written before the field reads
    /// as before, and a made row's wire form is unchanged.
    #[test]
    fn a_linux_person_s_row_records_a_uid() {
        let named = |account: &str| CreateSeat {
            name: account.into(),
            account: account.into(),
            autostart: false,
        };
        let mut ledger = Ledger::new();
        let made = ledger.allocate(request(1)).unwrap();
        let person = ledger
            .allocate_adopted_user(named("jdoe@corp.example.com"), 1500)
            .unwrap();
        assert_eq!(person.adopted_uid, Some(1500));
        assert!(person.adopted() && !person.owner && person.adopted_sid.is_none());
        assert!(ledger.validate().is_ok());

        let again = ledger.clone().allocate_adopted_user(named("jdoe"), 1500);
        assert!(matches!(
            again,
            Err(ValidationError::Duplicate {
                kind: "account uid",
                ..
            })
        ));
        for (account, uid) in [("jdoe", 999), ("jdoe", 60001), ("jdoe", 0x8000_0000)] {
            assert!(matches!(
                Ledger::new().allocate_adopted_user(named(account), uid),
                Err(ValidationError::InvalidUid)
            ));
        }
        assert!(matches!(
            Ledger::new().allocate_adopted_user(named("Jdoe"), 1500),
            Err(ValidationError::InvalidOwnerAccount)
        ));
        let mut both = person.clone();
        both.adopted_sid = Some("S-1-5-21-1-2-3-1001".into());
        let mut owner = person.clone();
        owner.owner = true;
        for bad in [both, owner] {
            let one = Ledger {
                seats: vec![bad],
                ..Ledger::new()
            };
            assert!(matches!(one.validate(), Err(ValidationError::InvalidUid)));
        }

        let wire = serde_json::to_value(&made).unwrap();
        assert!(wire.get("adopted_uid").is_none());
        assert_eq!(serde_json::to_value(&person).unwrap()["adopted_uid"], 1500);
        let old: Seat = serde_json::from_value(wire).unwrap();
        assert_eq!(old, made);
    }

    /// A profile may carry the owner's login as its name, whichever row came first.
    #[test]
    fn a_profile_named_like_the_owners_login_coexists_with_the_owner_row() {
        let named = |name: &str| CreateSeat {
            name: name.into(),
            account: format!("pf-{}", name.to_lowercase()),
            autostart: false,
        };
        let mut owner_first = Ledger::new();
        owner_first.allocate_owner("enrico").unwrap();
        owner_first.allocate(named("Enrico")).unwrap();

        let mut profile_first = Ledger::new();
        profile_first.allocate(named("Enrico")).unwrap();
        profile_first.allocate_owner("enrico").unwrap();

        let mut twice = owner_first.clone();
        assert!(matches!(
            twice.allocate(named("ENRICO")),
            Err(ValidationError::Duplicate { kind: "name", .. })
        ));
    }

    #[test]
    fn version_capacity_and_slot_are_validated() {
        let mut ledger = Ledger::new();
        ledger.schema_version = 2;
        assert!(matches!(
            ledger.validate(),
            Err(ValidationError::UnsupportedSchema(2))
        ));
        ledger.schema_version = LEDGER_SCHEMA_VERSION;
        ledger.allocate(request(0)).unwrap();
        ledger.seats[0].display_slot = 11;
        assert!(matches!(
            ledger.validate(),
            Err(ValidationError::InvalidDisplaySlot(11))
        ));
        ledger.seats.clear();
        ledger.seats = (0..=MAX_SEATS)
            .map(|n| Seat {
                id: SeatId::random(),
                name: format!("seat {n}"),
                account: format!("seat-{n}"),
                display_slot: 12,
                native_port: 1000 + n as u16,
                mgmt_port: 2000 + n as u16,
                autostart: false,
                runtime: RuntimeStatus::default(),
                owner: false,
                fingerprint: None,
                adopted_sid: None,
                adopted_uid: None,
                rdp_granted: false,
            })
            .collect();
        assert!(matches!(
            ledger.validate(),
            Err(ValidationError::TooManySeats(5))
        ));
    }
}
