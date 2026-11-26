//! The ledger: the supervisor's saved list of seats, and how each seat gets its ports and slot.
//!
//! A ledger has one schema version and at most four rows, one per seat. A row's id never
//! changes. Names, accounts, ports and display slots are unique across rows. Display slots are
//! 12 to 15, and the native, management and browser ports each come from a fixed pool of four.
//! Every value is checked again when the ledger is loaded.
//!
//! A row's runtime state is what the backend last observed. It does not replace the backend's
//! own knowledge of which process is the seat.
//!
//! A row's [`RowKind`] says whose account the seat runs on: an account the supervisor made, the
//! box owner's (Linux), or a person's own. A person's account is recorded by uid on Linux and by
//! SID on Windows, where its name follows the SAM naming rule. On disk and in every IPC frame
//! the kind is spelled as six flat fields. `SeatRecord` is the one place that reads and writes
//! them, and every rule about a kind is a method of [`RowKind`]. A seat on an existing account
//! takes a slot and ports like any other.

use crate::backend::Adopted;
use rand::Rng;
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::{BTreeMap, HashSet};
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
/// `LM20_UNLEN` (lmcons.h, SDK 10.0.26100): NetUserAdd rejects longer SAM account names.
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

/// Desk pads one seat may hold.
pub const MAX_LENT_PADS: usize = 4;
pub const MAX_PAD_KEY_LEN: usize = 128;
pub const MAX_PAD_NAME_LEN: usize = 64;

/// A desk pad lent to a seat: a game controller plugged into the box that only this seat's
/// account opens. `key` is what the backend minted for the hardware and finds it by again after
/// a replug or a reboot. The name and the USB ids are what it read at the lend, shown while the
/// pad is unplugged. `port` is where it found the pad then: while a second device carries the
/// same key, only the one there is the lent pad.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LentPad {
    pub key: String,
    pub name: String,
    #[serde(default)]
    pub vendor_id: u16,
    #[serde(default)]
    pub product_id: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<String>,
}

/// Whether a USB interface of this class, subclass and protocol may belong to a pad a seat
/// holds: HID, audio, CDC-ACM, CDC data or vendor-specific. ACM stops at protocol 6, short of
/// RNDIS's `ff`.
pub fn pad_interface_allowed(class: u8, subclass: u8, protocol: u8) -> bool {
    matches!(
        (class, subclass, protocol),
        (0x01 | 0x03 | 0x0a | 0xff, _, _) | (0x02, 0x02, 0x00..=0x06)
    )
}

/// Whether `key` can name a desk pad: letters, digits, `.`, `_` and `-`, starting with a letter
/// or digit, at most [`MAX_PAD_KEY_LEN`] long. The key lands in a file name and a URL path.
pub fn valid_pad_key(key: &str) -> bool {
    let mut bytes = key.bytes();
    key.len() <= MAX_PAD_KEY_LEN
        && bytes.next().is_some_and(|b| b.is_ascii_alphanumeric())
        && bytes.all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// A pad's name as a row keeps it: controls dropped, trimmed, cut at [`MAX_PAD_NAME_LEN`]
/// characters, and `Controller` when nothing is left. The text comes from the hardware.
pub fn pad_name(raw: &str) -> String {
    let clean: String = raw.chars().filter(|c| !c.is_control()).collect();
    let name: String = clean.trim().chars().take(MAX_PAD_NAME_LEN).collect();
    let name = name.trim_end();
    if name.is_empty() {
        "Controller".to_owned()
    } else {
        name.to_owned()
    }
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
    /// The host message `detail` is the English of, worded with `detail_params`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail_id: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub detail_params: BTreeMap<String, String>,
}

impl RuntimeStatus {
    pub fn stopped() -> Self {
        Self {
            state: RuntimeState::Stopped,
            ..Self::default()
        }
    }

    pub fn running() -> Self {
        Self {
            state: RuntimeState::Running,
            ..Self::default()
        }
    }

    /// Names the host message `detail` is the English of.
    pub fn worded(mut self, id: &str) -> Self {
        self.detail_id = Some(id.to_string());
        self
    }

    /// Fills the placeholder `name` of that message.
    pub fn param(mut self, name: &str, value: impl ToString) -> Self {
        self.detail_params
            .insert(name.to_string(), value.to_string());
        self
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
            ..Self::default()
        }
    }
}

/// A ledger row. The ledger and every IPC frame hold it as `SeatRecord` spells it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "SeatRecord", into = "SeatRecord")]
pub struct Seat {
    pub id: SeatId,
    pub name: String,
    pub account: String,
    pub display_slot: u8,
    pub native_port: u16,
    pub mgmt_port: u16,
    pub autostart: bool,
    pub runtime: RuntimeStatus,
    /// Whose account the row runs on.
    pub kind: RowKind,
    /// SHA-256 of the seat host's certificate DER, lowercase hex: the pin a client dials it
    /// with, carried by the box's `Redirect`. Learned while the seat runs; `None` before its
    /// host has minted an identity.
    pub fingerprint: Option<String>,
    /// The desk pads lent to this seat, at most [`MAX_LENT_PADS`]. A key is on one row only.
    pub lent: Vec<LentPad>,
}

impl Seat {
    /// The UDP port this seat's browser plane listens on ([`browser_port_for`]).
    pub fn browser_port(&self) -> Option<u16> {
        browser_port_for(self.native_port)
    }
}

/// Whose account a row runs on. `TryFrom<SeatRecord>` is the one place the ledger's six account
/// fields are read; every rule about a kind is a method here, an exhaustive match.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RowKind(Kind);

#[derive(Clone, Debug, PartialEq, Eq)]
enum Kind {
    /// An account the supervisor made and marks by comment.
    Made,
    /// The box owner's (Linux), never created or deleted by the ledger. `uid` is what their
    /// account resolved to when the row was last prepared; `None`: not prepared since then.
    Owner { uid: Option<u32> },
    /// A person's own Linux account, by the uid the daemon resolved at adopt. A name that later
    /// resolves to another uid is not the row's.
    LinuxPerson { uid: u32 },
    /// A person's own Windows account, by SID, with what adopt changed for release to put back.
    WindowsPerson { sid: String, adopted: Adopted },
}

impl RowKind {
    pub fn made() -> Self {
        Self(Kind::Made)
    }

    pub fn owner() -> Self {
        Self(Kind::Owner { uid: None })
    }

    pub fn linux_person(uid: u32) -> Self {
        Self(Kind::LinuxPerson { uid })
    }

    pub fn windows_person(sid: String) -> Self {
        Self(Kind::WindowsPerson {
            sid,
            adopted: Adopted::default(),
        })
    }

    /// The box owner's row (Linux).
    pub fn is_owner(&self) -> bool {
        match self.0 {
            Kind::Owner { .. } => true,
            Kind::Made | Kind::LinuxPerson { .. } | Kind::WindowsPerson { .. } => false,
        }
    }

    /// On a person's own account. The owner's row is not one: it is the owner's by role.
    pub fn is_person(&self) -> bool {
        match self.0 {
            Kind::LinuxPerson { .. } | Kind::WindowsPerson { .. } => true,
            Kind::Made | Kind::Owner { .. } => false,
        }
    }

    /// The supervisor made the account, so it may close and delete it and end its every
    /// process. Any other row's account is someone's, whichever platform adopted it.
    pub fn supervisor_owns_account(&self) -> bool {
        match self.0 {
            Kind::Made => true,
            Kind::Owner { .. } | Kind::LinuxPerson { .. } | Kind::WindowsPerson { .. } => false,
        }
    }

    /// Starts only on its person's proof: no autostart, keep-warm, door or dial starts it, and a
    /// plain Start answers `unlock_required`.
    pub fn starts_on_proof(&self) -> bool {
        match self.0 {
            Kind::LinuxPerson { .. } => true,
            Kind::Made | Kind::Owner { .. } | Kind::WindowsPerson { .. } => false,
        }
    }

    /// Keep-warm may start the row and the idle stop end it: not the owner's, the box's host,
    /// and not a Linux person's, whose desktop may hold unsaved work.
    pub fn upkeep(&self) -> bool {
        match self.0 {
            Kind::Made | Kind::WindowsPerson { .. } => true,
            Kind::Owner { .. } | Kind::LinuxPerson { .. } => false,
        }
    }

    /// The row's unit ends when its user sits down at the machine.
    pub fn yields_to_the_desk(&self) -> bool {
        match self.0 {
            Kind::Owner { .. } | Kind::LinuxPerson { .. } | Kind::WindowsPerson { .. } => true,
            Kind::Made => false,
        }
    }

    /// The uid the row's Linux account was recorded at, the owner's or a person's. No two rows
    /// hold one.
    pub fn uid(&self) -> Option<u32> {
        match self.0 {
            Kind::Owner { uid } => uid,
            Kind::LinuxPerson { uid } => Some(uid),
            Kind::Made | Kind::WindowsPerson { .. } => None,
        }
    }

    /// A Windows person's account SID, which outlives a rename of the account.
    pub fn sid(&self) -> Option<&str> {
        match &self.0 {
            Kind::WindowsPerson { sid, .. } => Some(sid),
            Kind::Made | Kind::Owner { .. } | Kind::LinuxPerson { .. } => None,
        }
    }

    /// What adopt changed on a Windows person's account, for release to take back alone.
    pub fn adopted(&self) -> Adopted {
        match self.0 {
            Kind::WindowsPerson { adopted, .. } => adopted,
            Kind::Made | Kind::Owner { .. } | Kind::LinuxPerson { .. } => Adopted::default(),
        }
    }

    /// The owner's row, prepared for the account at `uid`. Another kind stays as it is.
    pub(crate) fn prepared(self, uid: u32) -> Self {
        match self.0 {
            Kind::Owner { .. } => Self(Kind::Owner { uid: Some(uid) }),
            Kind::Made | Kind::LinuxPerson { .. } | Kind::WindowsPerson { .. } => self,
        }
    }

    /// A Windows person's row once adopt changed `adopted`. Another kind stays as it is.
    pub(crate) fn with_adopted(self, adopted: Adopted) -> Self {
        match self.0 {
            Kind::WindowsPerson { sid, .. } => Self(Kind::WindowsPerson { sid, adopted }),
            Kind::Made | Kind::Owner { .. } | Kind::LinuxPerson { .. } => self,
        }
    }

    /// Whether `account` can be this kind's: a made row's by [`validate_account`], the Linux
    /// owner's and a Linux person's by [`valid_owner_account`], the latter on an ordinary uid, a
    /// Windows person's by the SAM rule with a local user's SID.
    fn check(&self, account: &str) -> Result<(), ValidationError> {
        let linux = || {
            valid_owner_account(account)
                .then_some(())
                .ok_or(ValidationError::InvalidOwnerAccount)
        };
        match &self.0 {
            Kind::Made => validate_account(account),
            Kind::Owner { .. } => linux(),
            Kind::LinuxPerson { uid } if ordinary_uid(*uid) => linux(),
            Kind::LinuxPerson { .. } => Err(ValidationError::InvalidUid),
            Kind::WindowsPerson { sid, .. } => {
                validate_sam_account(account)?;
                validate_sid(sid)
            }
        }
    }
}

/// A row as the ledger and every IPC frame spell it: the kind as six flat fields, each written
/// only when set.
#[derive(Clone, Serialize, Deserialize)]
struct SeatRecord {
    id: SeatId,
    name: String,
    account: String,
    display_slot: u8,
    native_port: u16,
    mgmt_port: u16,
    #[serde(default)]
    autostart: bool,
    #[serde(default)]
    runtime: RuntimeStatus,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    owner: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    owner_uid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    fingerprint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    adopted_sid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    adopted_uid: Option<u32>,
    /// Adopt put the account in Remote Desktop Users.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    rdp_granted: bool,
    /// The account held `SeDenyInteractiveLogonRight` before adopt. Absent: release takes it off.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    console_deny_kept: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    lent: Vec<LentPad>,
}

impl TryFrom<SeatRecord> for Seat {
    type Error = ValidationError;

    /// A row adopted by both SID and uid, an owner's adopted by either, or an account its kind
    /// can't name is refused. Adopt flags on a kind that has none are dropped.
    fn try_from(record: SeatRecord) -> Result<Self, ValidationError> {
        let adopted = Adopted {
            rdp_granted: record.rdp_granted,
            console_deny_kept: record.console_deny_kept,
        };
        let kind = RowKind(
            match (record.owner, record.adopted_sid, record.adopted_uid) {
                (_, Some(_), Some(_)) | (true, _, Some(_)) => {
                    return Err(ValidationError::InvalidUid);
                }
                (true, Some(_), None) => return Err(ValidationError::OwnerWithSid),
                (true, None, None) => Kind::Owner {
                    uid: record.owner_uid,
                },
                (false, None, None) => Kind::Made,
                (false, None, Some(uid)) => Kind::LinuxPerson { uid },
                (false, Some(sid), None) => Kind::WindowsPerson { sid, adopted },
            },
        );
        kind.check(&record.account)?;
        Ok(Self {
            id: record.id,
            name: record.name,
            account: record.account,
            display_slot: record.display_slot,
            native_port: record.native_port,
            mgmt_port: record.mgmt_port,
            autostart: record.autostart,
            runtime: record.runtime,
            kind,
            fingerprint: record.fingerprint,
            lent: record.lent,
        })
    }
}

impl From<Seat> for SeatRecord {
    fn from(seat: Seat) -> Self {
        let none = Adopted::default();
        let (owner, owner_uid, adopted_sid, adopted_uid, adopted) = match seat.kind.0 {
            Kind::Made => (false, None, None, None, none),
            Kind::Owner { uid } => (true, uid, None, None, none),
            Kind::LinuxPerson { uid } => (false, None, None, Some(uid), none),
            Kind::WindowsPerson { sid, adopted } => (false, None, Some(sid), None, adopted),
        };
        Self {
            id: seat.id,
            name: seat.name,
            account: seat.account,
            display_slot: seat.display_slot,
            native_port: seat.native_port,
            mgmt_port: seat.mgmt_port,
            autostart: seat.autostart,
            runtime: seat.runtime,
            owner,
            owner_uid,
            fingerprint: seat.fingerprint,
            adopted_sid,
            adopted_uid,
            rdp_granted: adopted.rdp_granted,
            console_deny_kept: adopted.console_deny_kept,
            lent: seat.lent,
        }
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
        let mut pads = HashSet::new();
        if self
            .seats
            .iter()
            .filter(|seat| seat.kind.is_owner())
            .count()
            > 1
        {
            return Err(ValidationError::Duplicate {
                kind: "owner",
                value: "a second owner row".into(),
            });
        }
        for seat in &self.seats {
            SeatId::parse(seat.id.as_str())?;
            validate_name(&seat.name)?;
            seat.kind.check(&seat.account)?;
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
            if !seat.kind.is_owner() {
                unique(&mut names, seat.name.to_lowercase(), "name")?;
            }
            unique(&mut accounts, seat.account.to_lowercase(), "account")?;
            // A renamed account keeps its SID, so a second row for it differs only there.
            if let Some(sid) = seat.kind.sid() {
                unique(&mut sids, sid.to_owned(), "account SID")?;
            }
            // Two names can resolve to one user: `jdoe` and `jdoe@corp.example.com`.
            if let Some(uid) = seat.kind.uid() {
                unique(&mut uids, uid, "account uid")?;
            }
            unique(&mut slots, seat.display_slot, "display slot")?;
            unique(&mut ports, seat.native_port, "port")?;
            unique(&mut ports, seat.mgmt_port, "port")?;
            if seat.lent.len() > MAX_LENT_PADS {
                return Err(ValidationError::TooManyPads(seat.lent.len()));
            }
            for pad in &seat.lent {
                let port = pad.port.as_deref().is_none_or(valid_pad_key);
                if !valid_pad_key(&pad.key) || !port || pad_name(&pad.name) != pad.name {
                    return Err(ValidationError::InvalidPad);
                }
                unique(&mut pads, pad.key.clone(), "lent pad")?;
            }
        }
        Ok(())
    }

    /// The seat `key` is lent to.
    pub fn holder_of(&self, key: &str) -> Option<&Seat> {
        self.seats
            .iter()
            .find(|seat| seat.lent.iter().any(|pad| pad.key == key))
    }

    /// Lends `pad` to seat `id`, taking it from the row that held it. `false`: no such seat.
    /// A seat that holds [`MAX_LENT_PADS`] already is refused, and nothing changes.
    pub fn lend(&mut self, id: &SeatId, pad: LentPad) -> Result<bool, ValidationError> {
        let mut next = self.clone();
        next.take_back(&pad.key);
        let Some(seat) = next.seats.iter_mut().find(|seat| &seat.id == id) else {
            return Ok(false);
        };
        seat.lent.push(pad);
        next.validate()?;
        *self = next;
        Ok(true)
    }

    /// Takes `key` off the row that holds it. `false`: no row did.
    pub fn take_back(&mut self, key: &str) -> bool {
        let before = self.lent_count();
        for seat in &mut self.seats {
            seat.lent.retain(|pad| pad.key != key);
        }
        before != self.lent_count()
    }

    fn lent_count(&self) -> usize {
        self.seats.iter().map(|seat| seat.lent.len()).sum()
    }

    pub fn allocate(&mut self, request: CreateSeat) -> Result<Seat, ValidationError> {
        let id = loop {
            let id = SeatId::random();
            if !self.seats.iter().any(|seat| seat.id == id) {
                break id;
            }
        };
        self.allocate_with_id(request, id, RowKind::made())
    }

    /// The row for a person's own account `request.account`, whose SID is `sid`, with the slot
    /// and ports a seat gets. The caller checks that the SID names that account.
    pub fn allocate_adopted(
        &mut self,
        request: CreateSeat,
        sid: String,
    ) -> Result<Seat, ValidationError> {
        self.allocate_with_id(request, SeatId::random(), RowKind::windows_person(sid))
    }

    /// The row for a person's own Linux account `request.account`, which the daemon resolved to
    /// `uid`, with the slot and ports a seat gets.
    pub fn allocate_adopted_user(
        &mut self,
        request: CreateSeat,
        uid: u32,
    ) -> Result<Seat, ValidationError> {
        self.allocate_with_id(request, SeatId::random(), RowKind::linux_person(uid))
    }

    /// The owner's row for `account`, with the slot and ports a seat gets. The caller checks
    /// that the account exists and that no owner row does.
    pub fn allocate_owner(&mut self, account: &str) -> Result<Seat, ValidationError> {
        let request = CreateSeat {
            name: account.to_owned(),
            account: account.to_owned(),
            autostart: false,
        };
        self.allocate_with_id(request, SeatId::random(), RowKind::owner())
    }

    /// Adds the row of `kind` that `request` asks for under `id`.
    fn allocate_with_id(
        &mut self,
        request: CreateSeat,
        id: SeatId,
        kind: RowKind,
    ) -> Result<Seat, ValidationError> {
        self.validate()?;
        if self.seats.len() == MAX_SEATS {
            return Err(ValidationError::TooManySeats(MAX_SEATS + 1));
        }

        let request = request.normalized();
        validate_name(&request.name)?;
        kind.check(&request.account)?;
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
            kind,
            fingerprint: None,
            lent: Vec::new(),
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
        self.seats.iter().find(|seat| seat.kind.is_owner())
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
    #[error("the owner's row adopts no Windows account")]
    OwnerWithSid,
    #[error("display slot {0} is outside the reserved 12..=15 range")]
    InvalidDisplaySlot(u8),
    #[error("{0} port {1} is zero or reserved for the console host")]
    InvalidPort(&'static str, u16),
    #[error("duplicate {kind} '{value}'")]
    Duplicate { kind: &'static str, value: String },
    #[error("runtime status detail exceeds 512 bytes")]
    RuntimeDetailTooLong,
    #[error("a seat may hold at most four lent pads, got {0}")]
    TooManyPads(usize),
    #[error(
        "a lent pad needs a key and a port of letters, digits, '.', '_' or '-', and a clean name"
    )]
    InvalidPad,
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
            .allocate_with_id(request(1), first_id.clone(), RowKind::made())
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
        assert!(owner.kind.is_owner() && !first.kind.is_owner());
        assert_eq!(
            (owner.display_slot, owner.native_port, owner.mgmt_port),
            (13, 9780, 47996)
        );
        assert_eq!(owner.name, "enrico");
        assert_eq!(ledger.owner().map(|s| &s.id), Some(&owner.id));

        let mut second = ledger.clone();
        second.seats[0].kind = RowKind::owner();
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
        assert!(!old.kind.is_owner());
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
        assert_eq!(adopted.kind.sid(), Some(SID));
        assert!(!adopted.kind.adopted().rdp_granted && !adopted.kind.is_owner());
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
            kind: adopted.kind.clone().with_adopted(Adopted {
                rdp_granted: true,
                console_deny_kept: false,
            }),
            ..adopted
        };
        let wire = serde_json::to_value(&granted).unwrap();
        assert_eq!(
            (&wire["adopted_sid"], &wire["rdp_granted"]),
            (&SID.into(), &true.into())
        );
        assert_eq!(serde_json::from_value::<Seat>(wire).unwrap(), granted);
    }

    /// Release takes the console deny off only when adopt added it. A row without the field, as
    /// an older build saved it, takes it off; the field is written only when set.
    #[test]
    fn release_keeps_a_console_deny_the_account_held_before() {
        let request = CreateSeat {
            name: "Bärbel".into(),
            account: "Bärbel Ott".into(),
            autostart: false,
        };
        let row = Ledger::new()
            .allocate_adopted(request, "S-1-5-21-1-2-3-1001".into())
            .unwrap();
        let wire = serde_json::to_value(&row).unwrap();
        for (kept, releases) in [(None, true), (Some(false), true), (Some(true), false)] {
            let mut saved = wire.clone();
            if let Some(kept) = kept {
                saved["console_deny_kept"] = kept.into();
            }
            let seat: Seat = serde_json::from_value(saved).unwrap();
            assert_eq!(!seat.kind.adopted().console_deny_kept, releases, "{kept:?}");
            let written = serde_json::to_value(&seat).unwrap();
            assert_eq!(written.get("console_deny_kept").is_some(), !releases);
        }
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
        assert_eq!(person.kind.uid(), Some(1500));
        assert!(person.kind.is_person() && !person.kind.is_owner() && person.kind.sid().is_none());
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
        let record = SeatRecord::from(person.clone());
        let both = SeatRecord {
            adopted_sid: Some("S-1-5-21-1-2-3-1001".into()),
            ..record.clone()
        };
        let owner = SeatRecord {
            owner: true,
            ..record
        };
        for bad in [both, owner] {
            assert!(matches!(
                Seat::try_from(bad),
                Err(ValidationError::InvalidUid)
            ));
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
                kind: RowKind::made(),
                fingerprint: None,
                lent: Vec::new(),
            })
            .collect();
        assert!(matches!(
            ledger.validate(),
            Err(ValidationError::TooManySeats(5))
        ));
    }

    /// A record becomes a row of one kind or is refused by name; an adopt flag on a kind that has
    /// none is dropped, so a ledger holding one still loads.
    #[test]
    fn a_record_is_one_row_kind_or_refused() {
        const SID: &str = "S-1-5-21-1-2-3-1001";
        let made = SeatRecord::from(Ledger::new().allocate(request(1)).unwrap());
        let record = |account: &str, owner, sid: Option<&str>, uid| SeatRecord {
            account: account.into(),
            owner,
            adopted_sid: sid.map(Into::into),
            adopted_uid: uid,
            ..made.clone()
        };
        for (bad, error) in [
            (
                record("jdoe", false, Some(SID), Some(1500)),
                ValidationError::InvalidUid,
            ),
            (
                record("jdoe", true, None, Some(1500)),
                ValidationError::InvalidUid,
            ),
            (
                record("jdoe", false, None, Some(999)),
                ValidationError::InvalidUid,
            ),
            (
                record("Ben", true, Some(SID), None),
                ValidationError::OwnerWithSid,
            ),
            (
                record("jdoe@corp", false, None, None),
                ValidationError::InvalidAccount,
            ),
            (
                record("Jdoe", true, None, None),
                ValidationError::InvalidOwnerAccount,
            ),
            (
                record("Jdoe", false, None, Some(1500)),
                ValidationError::InvalidOwnerAccount,
            ),
            (
                record("a:b", false, Some(SID), None),
                ValidationError::InvalidSamAccount,
            ),
            (
                record("Ben", false, Some("S-1-5-32-544"), None),
                ValidationError::InvalidSid,
            ),
        ] {
            assert_eq!(Seat::try_from(bad).map(|s| s.kind), Err(error));
        }

        let stray = |record: SeatRecord| SeatRecord {
            owner_uid: Some(1000),
            rdp_granted: true,
            console_deny_kept: true,
            ..record
        };
        for (record, kind) in [
            (record("pf-seat-1", false, None, None), RowKind::made()),
            (
                record("jdoe", false, None, Some(1500)),
                RowKind::linux_person(1500),
            ),
            (
                record("enrico", true, None, None),
                RowKind::owner().prepared(1000),
            ),
        ] {
            assert_eq!(Seat::try_from(stray(record)).unwrap().kind, kind);
        }
    }
}
