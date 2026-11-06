//! Frames on the supervisor's named pipe or Unix socket, one request per connection.
//!
//! A frame is a four-byte big-endian length then JSON, capped at 64 KiB before allocation.
//! Requests and responses carry a schema version; commands and results are tagged enums;
//! errors have stable codes and a message. The pipe's DACL admits SYSTEM and Administrators
//! only, and the socket checks its peer's credentials, so the request carries no credential of
//! its own; an adopt, credential or unlock frame carries the account's password, which the
//! supervisor seals (Windows) or checks once (Linux) and forgets. Both ends hold each frame in a buffer
//! wiped when it drops. [`answer`] serves one request on any reader and writer, which keeps
//! dispatch testable without a pipe.

use crate::backend::PlatformBackend;
use crate::model::{CreateSeat, Seat, SeatId};
use crate::secret::Secret;
use crate::service::SeatService;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use zeroize::Zeroizing;

pub const CONTROL_SCHEMA_VERSION: u32 = 1;
pub const MAX_FRAME_BYTES: usize = 64 * 1024;

/// The pipe the Windows service serves. `\\.\pipe\` is the local namespace.
pub const PIPE_NAME: &str = r"\\.\pipe\punktfunk-seats";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Command {
    List,
    Create(CreateSeat),
    Start {
        id: SeatId,
    },
    Stop {
        id: SeatId,
    },
    Delete {
        id: SeatId,
    },
    Doctor,
    /// Whether seats are on, with what turning them on needs.
    Seating,
    /// Turn seats on. Remote Desktop stays reachable from this machine only unless
    /// `allow_rdp_from_network`. `prepare` first installs what Windows lacks and punktfunk may
    /// set up itself: the Session Host role and a licensing mode.
    Enable {
        allow_rdp_from_network: bool,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        prepare: bool,
    },
    /// Stop every seat and turn seats off. Only `keep_accounts: true` is served: an account
    /// goes with its profile.
    Disable {
        keep_accounts: bool,
    },
    /// Linux: the box owner's row for `account`, an ordinary user that already exists. Answers
    /// the row that exists, or creates it; `Delete` releases it and never removes the account.
    AdoptOwner {
        account: String,
    },
    /// Windows: a seat on `seat.account`, a person's standard account that already exists, whose
    /// SID is `sid`. `Delete` releases the account and never removes it.
    Adopt {
        seat: CreateSeat,
        sid: String,
        password: Secret,
    },
    /// Linux: a seat on `seat.account`, a person's own account that already exists, once PAM
    /// accepts `password`. The supervisor resolves the uid itself and keeps no password.
    /// `Delete` releases the row and never touches the account.
    AdoptUser {
        seat: CreateSeat,
        password: Secret,
    },
    /// Windows: an adopted seat's new password, verified and sealed in place of the old one.
    Credential {
        id: SeatId,
        password: Secret,
    },
    /// Linux: Flathub's app `id`, installed for every account on the box. The box owner may
    /// send it too.
    InstallFlatpak {
        id: String,
    },
    /// Linux: a box-wide Flatpak app removed; `purge` deletes root's data of it too. The box
    /// owner may send it too.
    RemoveFlatpak {
        id: String,
        purge: bool,
    },
    /// Linux, a person's row: let `device` onto it with `proof`, starting the session when the
    /// proof may. Answers `unlocked` only while the session runs and the device is admitted.
    Unlock {
        id: SeatId,
        device: Device,
        proof: Proof,
    },
    /// Linux, a person's row: what `fingerprint` must send to get on.
    UnlockState {
        id: SeatId,
        fingerprint: String,
    },
    /// Linux, a person's row: a fresh challenge for `fingerprint`'s enrolled key to sign.
    Challenge {
        id: SeatId,
        fingerprint: String,
    },
    /// Linux, a person's row: `fingerprint` disconnected, so its presence runs out soon.
    Left {
        id: SeatId,
        fingerprint: String,
    },
    /// Linux, a person's row: forget `fingerprint`, or every device when `None`.
    SignOut {
        id: SeatId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fingerprint: Option<String>,
    },
    /// Linux, a person's row: the devices it lists.
    Devices {
        id: SeatId,
    },
}

/// A device asking for a person's row: its certificate's SHA-256 as 64 lowercase hex, and the
/// name it paired with, at most 64 characters, shown and never trusted.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Device {
    pub fingerprint: String,
    pub name: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyAlg {
    /// ECDSA P-256 with SHA-256, the signature ASN.1 DER.
    P256,
    /// RSA 2048 with SHA-256, the signature PKCS#1 v1.5 or PSS.
    Rsa2048,
}

/// A device's unlock key, `public` in lowercase hex: the 65-byte uncompressed SEC1 point for
/// `p256`, a DER `RSAPublicKey` of at most 600 bytes for `rsa2048`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnlockKey {
    pub alg: KeyAlg,
    pub public: String,
}

/// What a device offers for a person's row.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Proof {
    /// Nothing: a device already admitted rejoins a running session.
    None,
    /// The account's password, checked by PAM. `stay_signed_in` keeps the device listed until it
    /// is signed out; `enroll` stores a key it may prove itself with later.
    Password {
        password: Secret,
        #[serde(default)]
        stay_signed_in: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        enroll: Option<UnlockKey>,
    },
    /// The enrolled key's signature, lowercase hex, at most 1024 bytes, over the live challenge.
    Key { signature: String },
}

impl Proof {
    /// The proof's wire name, for the audit line.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Password { .. } => "password",
            Self::Key { .. } => "key",
        }
    }
}

/// What a device must send to get onto a person's row.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Needs {
    None,
    Password,
    Key,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceKind {
    /// Listed until its presence ends.
    ForNow,
    /// Rejoins a running session without a proof until it is signed out.
    Stay,
    /// Proves itself with an enrolled key on every connect.
    Key,
}

/// A device a person's row lists, as the console shows it. Times are unix seconds.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceEntry {
    pub fingerprint: String,
    pub name: String,
    pub kind: DeviceKind,
    pub added_unix: i64,
    pub last_unix: i64,
}

/// A Flatpak app id the supervisor hands to `flatpak`: reverse-DNS, `[A-Za-z0-9._-]`, at least
/// one dot, at most 255 bytes, and no leading `-`, which `flatpak` would read as an option.
pub fn valid_flatpak_id(id: &str) -> bool {
    id.len() <= 255
        && id.contains('.')
        && !id.starts_with('-')
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub schema_version: u32,
    pub command: Command,
}

impl Request {
    pub fn new(command: Command) -> Self {
        Self {
            schema_version: CONTROL_SCHEMA_VERSION,
            command,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CommandResult {
    List {
        seats: Vec<Seat>,
    },
    Created {
        seat: Seat,
    },
    Started {
        seat: Seat,
    },
    Stopped {
        seat: Seat,
    },
    Deleted {
        id: SeatId,
    },
    Doctor {
        report: DoctorReport,
    },
    Seating {
        status: SeatingStatus,
    },
    Resealed {
        seat: Seat,
    },
    /// A box-wide Flatpak change done: the app's version now, `None` once removed.
    Flatpak {
        id: String,
        #[serde(default)]
        version: Option<String>,
    },
    /// The device is admitted and the session runs; `started` when this unlock started it.
    Unlocked {
        seat: Seat,
        started: bool,
    },
    UnlockState {
        needs: Needs,
    },
    /// 32 random bytes as 64 lowercase hex, for `seat`'s id to be signed with.
    Challenge {
        challenge: String,
        seat: SeatId,
    },
    Left,
    SignedOut,
    Devices {
        devices: Vec<DeviceEntry>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Response {
    Success {
        schema_version: u32,
        result: CommandResult,
    },
    Error {
        schema_version: u32,
        error: ApiError,
    },
}

impl Response {
    pub fn success(result: CommandResult) -> Self {
        Self::Success {
            schema_version: CONTROL_SCHEMA_VERSION,
            result,
        }
    }

    pub fn error(error: ApiError) -> Self {
        Self::Error {
            schema_version: CONTROL_SCHEMA_VERSION,
            error,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiError {
    pub code: ErrorCode,
    pub message: String,
}

impl ApiError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    SchemaVersion,
    InvalidRequest,
    Capacity,
    Conflict,
    NotFound,
    Backend,
    Persistence,
    FrameTooLarge,
    MalformedFrame,
    Transport,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DoctorReport {
    pub healthy: bool,
    pub diagnostics: Vec<Diagnostic>,
}

/// Whether seats are on and what the checks for turning them on found. A check that failed is
/// an error-level [`Diagnostic`] whose message is one plain sentence for the operator.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeatingStatus {
    pub enabled: bool,
    pub checks: Vec<Diagnostic>,
    /// Windows: seats are on and the network may reach Remote Desktop.
    #[serde(default)]
    pub allow_rdp_from_network: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub level: DiagnosticLevel,
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seat_id: Option<SeatId>,
}

impl Diagnostic {
    pub fn info(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            level: DiagnosticLevel::Info,
            code: code.into(),
            message: message.into(),
            seat_id: None,
        }
    }

    pub fn warning(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            level: DiagnosticLevel::Warning,
            ..Self::info(code, message)
        }
    }

    pub fn error(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            level: DiagnosticLevel::Error,
            code: code.into(),
            message: message.into(),
            seat_id: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticLevel {
    Info,
    Warning,
    Error,
}

pub fn read_json_frame<R, T>(reader: &mut R) -> Result<T, FrameError>
where
    R: Read,
    T: DeserializeOwned,
{
    let bytes = read_frame_bytes(reader)?;
    serde_json::from_slice(&bytes).map_err(FrameError::Decode)
}

/// Encodes `value` into one wiped buffer of its exact size, measured first, so a frame that
/// carries a password leaves no copy behind a growing buffer.
pub fn write_json_frame<W, T>(writer: &mut W, value: &T) -> Result<(), FrameError>
where
    W: Write,
    T: Serialize,
{
    let mut length = Measure(0);
    serde_json::to_writer(&mut length, value).map_err(FrameError::Encode)?;
    if length.0 > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge(length.0));
    }
    let mut bytes = Zeroizing::new(vec![0_u8; length.0]);
    let mut rest = bytes.as_mut_slice();
    serde_json::to_writer(&mut rest, value).map_err(FrameError::Encode)?;
    let written = length.0 - rest.len();
    write_frame_bytes(writer, &bytes[..written])
}

/// Counts the bytes written to it and keeps none.
struct Measure(usize);

impl Write for Measure {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self.0.saturating_add(bytes.len());
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// One frame's payload, in a buffer wiped when it drops: it may carry a password.
pub fn read_frame_bytes<R: Read>(reader: &mut R) -> Result<Zeroizing<Vec<u8>>, FrameError> {
    let mut prefix = [0_u8; 4];
    reader.read_exact(&mut prefix)?;
    let length = u32::from_be_bytes(prefix) as usize;
    if length > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge(length));
    }
    let mut payload = Zeroizing::new(vec![0_u8; length]);
    reader.read_exact(&mut payload)?;
    Ok(payload)
}

pub fn write_frame_bytes<W: Write>(writer: &mut W, payload: &[u8]) -> Result<(), FrameError> {
    if payload.len() > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge(payload.len()));
    }
    writer.write_all(&(payload.len() as u32).to_be_bytes())?;
    writer.write_all(payload)?;
    writer.flush()?;
    Ok(())
}

/// Serve one request from `stream`: read it, check its schema, dispatch it, write the answer.
/// A frame that does not decode is answered with its error code before the connection ends.
pub fn answer<B, S>(service: &SeatService<B>, stream: &mut S) -> Result<(), FrameError>
where
    B: PlatformBackend,
    S: Read + Write,
{
    answer_if(service, stream, |_| true)
}

/// [`answer`] for a peer that may send only the commands `admit` passes. Any other is closed
/// unanswered, as a peer that may send nothing is.
pub fn answer_if<B, S>(
    service: &SeatService<B>,
    stream: &mut S,
    admit: impl FnOnce(&Command) -> bool,
) -> Result<(), FrameError>
where
    B: PlatformBackend,
    S: Read + Write,
{
    let response = match read_json_frame::<_, Request>(stream) {
        Err(error) => Response::error(frame_api_error(&error)),
        Ok(request) if !admit(&request.command) => return Ok(()),
        Ok(request) if request.schema_version != CONTROL_SCHEMA_VERSION => {
            Response::error(ApiError::new(
                ErrorCode::SchemaVersion,
                format!(
                    "control schema {} is unsupported; expected {}",
                    request.schema_version, CONTROL_SCHEMA_VERSION
                ),
            ))
        }
        Ok(request) => match service.dispatch(request.command) {
            Ok(result) => Response::success(result),
            Err(error) => Response::error(error),
        },
    };
    write_json_frame(stream, &response)
}

fn frame_api_error(error: &FrameError) -> ApiError {
    match error {
        FrameError::TooLarge(_) => ApiError::new(ErrorCode::FrameTooLarge, error.to_string()),
        _ => ApiError::new(ErrorCode::MalformedFrame, error.to_string()),
    }
}

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("control frame I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("control frame is {0} bytes; the limit is 65536")]
    TooLarge(usize),
    #[error("control JSON encoding failed: {0}")]
    Encode(serde_json::Error),
    #[error("control JSON decoding failed: {0}")]
    Decode(serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn framing_accepts_the_cap_and_rejects_one_byte_more() {
        let exact = vec![7_u8; MAX_FRAME_BYTES];
        let mut framed = Vec::new();
        write_frame_bytes(&mut framed, &exact).unwrap();
        assert_eq!(*read_frame_bytes(&mut Cursor::new(framed)).unwrap(), exact);

        let oversized = vec![0_u8; MAX_FRAME_BYTES + 1];
        assert!(matches!(
            write_frame_bytes(&mut Vec::new(), &oversized),
            Err(FrameError::TooLarge(size)) if size == MAX_FRAME_BYTES + 1
        ));
        let prefix_only = ((MAX_FRAME_BYTES + 1) as u32).to_be_bytes();
        assert!(matches!(
            read_frame_bytes(&mut Cursor::new(prefix_only)),
            Err(FrameError::TooLarge(size)) if size == MAX_FRAME_BYTES + 1
        ));
    }

    #[test]
    fn a_request_round_trips() {
        let request = Request::new(Command::List);
        let mut frame = Vec::new();
        write_json_frame(&mut frame, &request).unwrap();
        let decoded: Request = read_json_frame(&mut Cursor::new(frame)).unwrap();
        assert_eq!(decoded, request);
    }

    /// The seating commands and their answer cross a frame unchanged, and the wire names are
    /// the ones the console host and the supervisor share. A password rides the frame and never
    /// a command's `Debug`.
    #[test]
    fn the_seating_frames_round_trip() {
        let password = || Secret::new("hunter2 Bärbel".into());
        for (command, wire) in [
            (Command::Seating, r#"{"type":"seating"}"#),
            (
                Command::Enable {
                    allow_rdp_from_network: true,
                    prepare: false,
                },
                r#"{"type":"enable","allow_rdp_from_network":true}"#,
            ),
            (
                Command::Disable {
                    keep_accounts: true,
                },
                r#"{"type":"disable","keep_accounts":true}"#,
            ),
            (
                Command::AdoptOwner {
                    account: "enrico".into(),
                },
                r#"{"type":"adopt_owner","account":"enrico"}"#,
            ),
            (
                Command::Adopt {
                    seat: CreateSeat {
                        name: "Ben".into(),
                        account: "Ben Ott".into(),
                        autostart: false,
                    },
                    sid: "S-1-5-21-1-2-3-1001".into(),
                    password: password(),
                },
                r#"{"type":"adopt","seat":{"name":"Ben","account":"Ben Ott","autostart":false},"sid":"S-1-5-21-1-2-3-1001","password":"hunter2 Bärbel"}"#,
            ),
            (
                Command::AdoptUser {
                    seat: CreateSeat {
                        name: "Ben".into(),
                        account: "jdoe@corp.example.com".into(),
                        autostart: false,
                    },
                    password: password(),
                },
                r#"{"type":"adopt_user","seat":{"name":"Ben","account":"jdoe@corp.example.com","autostart":false},"password":"hunter2 Bärbel"}"#,
            ),
            (
                Command::Credential {
                    id: SeatId::parse("01".repeat(16)).unwrap(),
                    password: password(),
                },
                r#"{"type":"credential","id":"01010101010101010101010101010101","password":"hunter2 Bärbel"}"#,
            ),
        ] {
            assert_eq!(serde_json::to_string(&command).unwrap(), wire);
            assert!(!format!("{command:?}").contains("hunter2"), "{wire}");
            let mut frame = Vec::new();
            write_json_frame(&mut frame, &Request::new(command.clone())).unwrap();
            let plain = serde_json::to_vec(&Request::new(command.clone())).unwrap();
            assert_eq!(frame[..4], (plain.len() as u32).to_be_bytes(), "{wire}");
            assert_eq!(
                frame[4..],
                plain,
                "the wiped encoding is byte for byte the plain one"
            );
            let decoded: Request = read_json_frame(&mut Cursor::new(frame)).unwrap();
            assert_eq!(decoded.command, command);
        }
        let answer = Response::success(CommandResult::Seating {
            status: SeatingStatus {
                enabled: false,
                checks: vec![Diagnostic::error("no_gpu", "No graphics card was found.")],
                allow_rdp_from_network: false,
            },
        });
        let mut frame = Vec::new();
        write_json_frame(&mut frame, &answer).unwrap();
        assert_eq!(
            read_json_frame::<_, Response>(&mut Cursor::new(frame)).unwrap(),
            answer
        );
    }

    /// The unlock commands and answers as the door and the supervisor share them (CONTRACT §4).
    /// A password rides the frame and never a command's `Debug`.
    #[test]
    fn the_unlock_frames_round_trip() {
        let id = || SeatId::parse("01".repeat(16)).unwrap();
        let fp = "ab".repeat(32);
        let device = Device {
            fingerprint: fp.clone(),
            name: "Jane's iPhone".into(),
        };
        let key = UnlockKey {
            alg: KeyAlg::P256,
            public: "04".into(),
        };
        let unlock = |proof| Command::Unlock {
            id: id(),
            device: device.clone(),
            proof,
        };
        let head = format!(
            r#"{{"type":"unlock","id":"{}","device":{{"fingerprint":"{fp}","name":"Jane's iPhone"}},"proof":"#,
            id()
        );
        for (command, wire) in [
            (unlock(Proof::None), format!(r#"{head}{{"kind":"none"}}}}"#)),
            (
                unlock(Proof::Password {
                    password: Secret::new("hunter2 Bärbel".into()),
                    stay_signed_in: true,
                    enroll: Some(key.clone()),
                }),
                format!(
                    r#"{head}{{"kind":"password","password":"hunter2 Bärbel","stay_signed_in":true,"enroll":{{"alg":"p256","public":"04"}}}}}}"#
                ),
            ),
            (
                unlock(Proof::Key {
                    signature: "3045".into(),
                }),
                format!(r#"{head}{{"kind":"key","signature":"3045"}}}}"#),
            ),
            (
                Command::UnlockState {
                    id: id(),
                    fingerprint: fp.clone(),
                },
                format!(
                    r#"{{"type":"unlock_state","id":"{}","fingerprint":"{fp}"}}"#,
                    id()
                ),
            ),
            (
                Command::Challenge {
                    id: id(),
                    fingerprint: fp.clone(),
                },
                format!(
                    r#"{{"type":"challenge","id":"{}","fingerprint":"{fp}"}}"#,
                    id()
                ),
            ),
            (
                Command::Left {
                    id: id(),
                    fingerprint: fp.clone(),
                },
                format!(r#"{{"type":"left","id":"{}","fingerprint":"{fp}"}}"#, id()),
            ),
            (
                Command::SignOut {
                    id: id(),
                    fingerprint: Some(fp.clone()),
                },
                format!(
                    r#"{{"type":"sign_out","id":"{}","fingerprint":"{fp}"}}"#,
                    id()
                ),
            ),
            (
                Command::SignOut {
                    id: id(),
                    fingerprint: None,
                },
                format!(r#"{{"type":"sign_out","id":"{}"}}"#, id()),
            ),
            (
                Command::Devices { id: id() },
                format!(r#"{{"type":"devices","id":"{}"}}"#, id()),
            ),
        ] {
            assert_eq!(serde_json::to_string(&command).unwrap(), wire);
            assert_eq!(serde_json::from_str::<Command>(&wire).unwrap(), command);
            assert!(!format!("{command:?}").contains("hunter2"), "{wire}");
        }
        let bare = format!(r#"{head}{{"kind":"password","password":"pw"}}}}"#);
        let Command::Unlock { proof, .. } = serde_json::from_str(&bare).unwrap() else {
            panic!("an unlock")
        };
        assert_eq!(
            proof,
            Proof::Password {
                password: Secret::new("pw".into()),
                stay_signed_in: false,
                enroll: None
            }
        );
        let rsa = r#"{"alg":"rsa2048","public":"30"}"#;
        assert_eq!(
            serde_json::from_str::<UnlockKey>(rsa).unwrap().alg,
            KeyAlg::Rsa2048
        );

        let entry = DeviceEntry {
            fingerprint: fp.clone(),
            name: "TV".into(),
            kind: DeviceKind::ForNow,
            added_unix: 1,
            last_unix: 2,
        };
        for (result, wire) in [
            (
                CommandResult::UnlockState { needs: Needs::Key },
                r#"{"kind":"unlock_state","needs":"key"}"#.to_owned(),
            ),
            (
                CommandResult::Challenge {
                    challenge: "00".repeat(32),
                    seat: id(),
                },
                format!(
                    r#"{{"kind":"challenge","challenge":"{}","seat":"{}"}}"#,
                    "00".repeat(32),
                    id()
                ),
            ),
            (CommandResult::Left, r#"{"kind":"left"}"#.to_owned()),
            (
                CommandResult::SignedOut,
                r#"{"kind":"signed_out"}"#.to_owned(),
            ),
            (
                CommandResult::Devices {
                    devices: vec![entry],
                },
                format!(
                    r#"{{"kind":"devices","devices":[{{"fingerprint":"{fp}","name":"TV","kind":"for_now","added_unix":1,"last_unix":2}}]}}"#
                ),
            ),
        ] {
            assert_eq!(serde_json::to_string(&result).unwrap(), wire);
            assert_eq!(
                serde_json::from_str::<CommandResult>(&wire).unwrap(),
                result
            );
        }
        let seat = crate::model::Ledger::new()
            .allocate_adopted_user(
                CreateSeat {
                    name: "Jane".into(),
                    account: "jdoe".into(),
                    autostart: false,
                },
                1500,
            )
            .unwrap();
        let unlocked = Response::success(CommandResult::Unlocked {
            seat,
            started: true,
        });
        let wire = serde_json::to_value(&unlocked).unwrap();
        assert_eq!(wire["result"]["kind"], "unlocked");
        assert_eq!(wire["result"]["started"], true);
        let mut frame = Vec::new();
        write_json_frame(&mut frame, &unlocked).unwrap();
        assert_eq!(
            read_json_frame::<_, Response>(&mut Cursor::new(frame)).unwrap(),
            unlocked
        );
        for kind in [DeviceKind::Stay, DeviceKind::Key] {
            let text = serde_json::to_string(&kind).unwrap();
            assert_eq!(serde_json::from_str::<DeviceKind>(&text).unwrap(), kind);
        }
        assert_eq!(serde_json::to_string(&Needs::None).unwrap(), r#""none""#);
        assert_eq!(
            serde_json::to_string(&Needs::Password).unwrap(),
            r#""password""#
        );
    }

    /// The Flatpak commands and their answer as the box host and the supervisor share them.
    #[test]
    fn the_flatpak_frames_round_trip() {
        for (command, wire) in [
            (
                Command::InstallFlatpak {
                    id: "net.pcsx2.PCSX2".into(),
                },
                r#"{"type":"install_flatpak","id":"net.pcsx2.PCSX2"}"#,
            ),
            (
                Command::RemoveFlatpak {
                    id: "org.DolphinEmu.dolphin-emu".into(),
                    purge: true,
                },
                r#"{"type":"remove_flatpak","id":"org.DolphinEmu.dolphin-emu","purge":true}"#,
            ),
        ] {
            assert_eq!(serde_json::to_string(&command).unwrap(), wire);
            assert_eq!(serde_json::from_str::<Command>(wire).unwrap(), command);
        }
        let done = CommandResult::Flatpak {
            id: "net.pcsx2.PCSX2".into(),
            version: Some("v2.2.0".into()),
        };
        let wire = r#"{"kind":"flatpak","id":"net.pcsx2.PCSX2","version":"v2.2.0"}"#;
        assert_eq!(serde_json::to_string(&done).unwrap(), wire);
        let removed: CommandResult =
            serde_json::from_str(r#"{"kind":"flatpak","id":"a.b"}"#).unwrap();
        assert_eq!(
            removed,
            CommandResult::Flatpak {
                id: "a.b".into(),
                version: None
            }
        );
    }

    /// Only a reverse-DNS name reaches `flatpak`: nothing it reads as an option, a path, a
    /// remote or a second argument.
    #[test]
    fn a_flatpak_id_is_a_reverse_dns_name() {
        for good in [
            "net.pcsx2.PCSX2",
            "org.DolphinEmu.dolphin-emu",
            "info.cemu.Cemu",
            "io.github.ryubing_.Ryujinx",
            "a.b",
        ] {
            assert!(valid_flatpak_id(good), "{good}");
        }
        let long = format!("a.{}", "b".repeat(253));
        assert!(valid_flatpak_id(&long));
        let too_long = format!("a.{}", "b".repeat(254));
        for bad in [
            "",
            "PCSX2",
            "-y.evil",
            "--from=x.y",
            "net.pcsx2.PCSX2 org.evil.App",
            "flathub:net.pcsx2.PCSX2",
            "../../etc.passwd",
            "net/pcsx2.PCSX2",
            "net.pcsx2.PCSX2\n",
            "net.pcsx2.PCSX2;",
            "nét.pcsx2.PCSX2",
            too_long.as_str(),
        ] {
            assert!(!valid_flatpak_id(bad), "{bad:?}");
        }
    }

    /// One duplex stream for [`answer`]: reads from `input`, writes to `output`.
    struct Duplex {
        input: Cursor<Vec<u8>>,
        output: Vec<u8>,
    }

    impl Read for Duplex {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.input.read(buf)
        }
    }

    impl Write for Duplex {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.output.write(buf)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn ask(service: &SeatService<crate::UnsupportedBackend>, frame: Vec<u8>) -> Response {
        let mut stream = Duplex {
            input: Cursor::new(frame),
            output: Vec::new(),
        };
        answer(service, &mut stream).unwrap();
        read_json_frame(&mut Cursor::new(stream.output)).unwrap()
    }

    /// Without a platform every seating command answers the unsupported error, and a disable
    /// that would delete accounts is refused before it gets that far.
    #[test]
    fn seating_is_unavailable_without_a_platform() {
        let temp = tempfile::tempdir().unwrap();
        let service = SeatService::open(temp.path(), crate::UnsupportedBackend).unwrap();
        let refuse = |command: Command| {
            let mut frame = Vec::new();
            write_json_frame(&mut frame, &Request::new(command)).unwrap();
            let Response::Error { error, .. } = ask(&service, frame) else {
                panic!("a platform without seats refuses")
            };
            error
        };
        for command in [
            Command::Seating,
            Command::Enable {
                allow_rdp_from_network: false,
                prepare: false,
            },
            Command::Disable {
                keep_accounts: true,
            },
        ] {
            let error = refuse(command);
            assert_eq!(error.code, ErrorCode::Backend);
            assert!(
                error.message.starts_with("platform_unavailable"),
                "{}",
                error.message
            );
        }
        let error = refuse(Command::Disable {
            keep_accounts: false,
        });
        assert_eq!(error.code, ErrorCode::InvalidRequest);
    }

    /// A list on an empty ledger answers with no seats; a wrong schema and a frame that is
    /// not JSON each answer with their own code.
    #[test]
    fn answer_serves_one_request() {
        let temp = tempfile::tempdir().unwrap();
        let service = SeatService::open(temp.path(), crate::UnsupportedBackend).unwrap();
        let mut list = Vec::new();
        write_json_frame(&mut list, &Request::new(Command::List)).unwrap();
        assert_eq!(
            ask(&service, list),
            Response::success(CommandResult::List { seats: Vec::new() })
        );

        let mut future = Vec::new();
        let newer = Request {
            schema_version: CONTROL_SCHEMA_VERSION + 1,
            command: Command::List,
        };
        write_json_frame(&mut future, &newer).unwrap();
        let Response::Error { error, .. } = ask(&service, future) else {
            panic!("a newer schema is refused")
        };
        assert_eq!(error.code, ErrorCode::SchemaVersion);

        let mut garbage = Vec::new();
        write_frame_bytes(&mut garbage, b"not json").unwrap();
        let Response::Error { error, .. } = ask(&service, garbage) else {
            panic!("a bad frame is refused")
        };
        assert_eq!(error.code, ErrorCode::MalformedFrame);
    }

    /// A command the peer may not send gets no answer at all; one it may is served.
    #[test]
    fn a_command_the_peer_may_not_send_is_closed_unanswered() {
        let temp = tempfile::tempdir().unwrap();
        let service = SeatService::open(temp.path(), crate::UnsupportedBackend).unwrap();
        let flatpaks_only = |c: &Command| matches!(c, Command::InstallFlatpak { .. });
        let run = |command: Command| {
            let mut frame = Vec::new();
            write_json_frame(&mut frame, &Request::new(command)).unwrap();
            let mut stream = Duplex {
                input: Cursor::new(frame),
                output: Vec::new(),
            };
            answer_if(&service, &mut stream, flatpaks_only).unwrap();
            stream.output
        };
        assert!(run(Command::List).is_empty());
        let answered = run(Command::InstallFlatpak { id: "a.b".into() });
        let Response::Error { error, .. } = read_json_frame(&mut Cursor::new(answered)).unwrap()
        else {
            panic!("no platform installs Flatpaks here")
        };
        assert_eq!(error.code, ErrorCode::Backend);
    }
}
