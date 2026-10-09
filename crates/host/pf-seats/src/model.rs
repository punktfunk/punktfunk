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
//! adopted rather than created. It takes a slot and ports like any seat.

use rand::Rng;
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::HashSet;
use std::fmt;
use std::str::FromStr;

pub const LEDGER_SCHEMA_VERSION: u32 = 1;
pub const MAX_SEATS: usize = 4;
pub const DISPLAY_SLOTS: [u8; MAX_SEATS] = [12, 13, 14, 15];
/// The console host's own defaults. A seat on either could never run beside
/// the console host.
pub const RESERVED_PORTS: [u16; 2] = [CONSOLE_NATIVE_PORT, CONSOLE_MGMT_PORT];
pub const CONSOLE_NATIVE_PORT: u16 = 9777;
pub const CONSOLE_MGMT_PORT: u16 = 47990;
/// After the browser plane's 9778.
pub const DEFAULT_NATIVE_PORTS: [u16; MAX_SEATS] = [9779, 9780, 9781, 9782];
pub const DEFAULT_MGMT_PORTS: [u16; MAX_SEATS] = [47995, 47996, 47997, 47998];
/// `LM20_UNLEN`: NetUserAdd rejects longer SAM account names.
pub const MAX_ACCOUNT_LEN: usize = 20;
/// Bytes of backend error text a seat's runtime status keeps.
pub const MAX_RUNTIME_DETAIL: usize = 512;

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
            validate_account(&seat.account)?;
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
            unique(&mut names, seat.name.to_lowercase(), "name")?;
            unique(&mut accounts, seat.account.to_lowercase(), "account")?;
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
        self.allocate_with_id(request, id, false)
    }

    /// The owner's row for `account`, with the slot and ports a seat gets. The caller checks
    /// that the account exists and that no owner row does.
    pub fn allocate_owner(&mut self, account: &str) -> Result<Seat, ValidationError> {
        let request = CreateSeat {
            name: account.to_owned(),
            account: account.to_owned(),
            autostart: false,
        };
        self.allocate_with_id(request, SeatId::random(), true)
    }

    fn allocate_with_id(
        &mut self,
        request: CreateSeat,
        id: SeatId,
        owner: bool,
    ) -> Result<Seat, ValidationError> {
        self.validate()?;
        if self.seats.len() == MAX_SEATS {
            return Err(ValidationError::TooManySeats(MAX_SEATS + 1));
        }

        let request = request.normalized();
        validate_name(&request.name)?;
        validate_account(&request.account)?;
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
        };
        self.seats.push(seat.clone());
        if let Err(error) = self.validate() {
            self.seats.pop();
            return Err(error);
        }
        Ok(seat)
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
            .allocate_with_id(request(1), first_id.clone(), false)
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

    #[test]
    fn console_host_and_control_ports_are_never_seat_ports() {
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
            })
            .collect();
        assert!(matches!(
            ledger.validate(),
            Err(ValidationError::TooManySeats(5))
        ));
    }
}
