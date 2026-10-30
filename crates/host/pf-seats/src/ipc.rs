//! Frames on the supervisor's named pipe or Unix socket, one request per connection.
//!
//! A frame is a four-byte big-endian length then JSON, capped at 64 KiB before allocation.
//! Requests and responses carry a schema version; commands and results are tagged enums;
//! errors have stable codes and a message. The pipe's DACL admits SYSTEM and Administrators
//! only, and the socket checks its peer's credentials, so the request carries no credential of
//! its own; an adopt or credential frame carries the account's password, which the supervisor
//! seals and forgets. [`answer`] serves one request on any reader and writer, which keeps
//! dispatch testable without a pipe.

use crate::backend::PlatformBackend;
use crate::model::{CreateSeat, Seat, SeatId};
use crate::secret::Secret;
use crate::service::SeatService;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};

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

pub fn write_json_frame<W, T>(writer: &mut W, value: &T) -> Result<(), FrameError>
where
    W: Write,
    T: Serialize,
{
    let bytes = serde_json::to_vec(value).map_err(FrameError::Encode)?;
    write_frame_bytes(writer, &bytes)
}

pub fn read_frame_bytes<R: Read>(reader: &mut R) -> Result<Vec<u8>, FrameError> {
    let mut prefix = [0_u8; 4];
    reader.read_exact(&mut prefix)?;
    let length = u32::from_be_bytes(prefix) as usize;
    if length > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge(length));
    }
    let mut payload = vec![0_u8; length];
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
        assert_eq!(read_frame_bytes(&mut Cursor::new(framed)).unwrap(), exact);

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
