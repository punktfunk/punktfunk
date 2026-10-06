//! Frames on the supervisor's named pipe or Unix socket, one request per connection.
//!
//! A frame is a four-byte big-endian length then JSON, capped at 64 KiB before allocation.
//! Requests and responses carry a schema version; commands and results are tagged enums;
//! errors have stable codes and a message. The pipe's DACL admits SYSTEM and Administrators
//! only, and the socket checks its peer's credentials, so a request carries no credential.
//! [`answer`] serves one request on any reader and writer, which keeps dispatch testable
//! without a pipe.

use crate::backend::PlatformBackend;
use crate::model::{CreateSeat, Seat, SeatId};
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
    /// `allow_rdp_from_network`.
    Enable {
        allow_rdp_from_network: bool,
    },
    /// Stop every seat and turn seats off. Only `keep_accounts: true` is served: an account
    /// goes with its profile.
    Disable {
        keep_accounts: bool,
    },
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
    List { seats: Vec<Seat> },
    Created { seat: Seat },
    Started { seat: Seat },
    Stopped { seat: Seat },
    Deleted { id: SeatId },
    Doctor { report: DoctorReport },
    Seating { status: SeatingStatus },
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
    let response = match read_json_frame::<_, Request>(stream) {
        Err(error) => Response::error(frame_api_error(&error)),
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
    /// the ones the console host and the supervisor share.
    #[test]
    fn the_seating_frames_round_trip() {
        for (command, wire) in [
            (Command::Seating, r#"{"type":"seating"}"#),
            (
                Command::Enable {
                    allow_rdp_from_network: true,
                },
                r#"{"type":"enable","allow_rdp_from_network":true}"#,
            ),
            (
                Command::Disable {
                    keep_accounts: true,
                },
                r#"{"type":"disable","keep_accounts":true}"#,
            ),
        ] {
            assert_eq!(serde_json::to_string(&command).unwrap(), wire);
            let mut frame = Vec::new();
            write_json_frame(&mut frame, &Request::new(command.clone())).unwrap();
            let decoded: Request = read_json_frame(&mut Cursor::new(frame)).unwrap();
            assert_eq!(decoded.command, command);
        }
        let answer = Response::success(CommandResult::Seating {
            status: SeatingStatus {
                enabled: false,
                checks: vec![Diagnostic::error("no_gpu", "No graphics card was found.")],
            },
        });
        let mut frame = Vec::new();
        write_json_frame(&mut frame, &answer).unwrap();
        assert_eq!(
            read_json_frame::<_, Response>(&mut Cursor::new(frame)).unwrap(),
            answer
        );
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
}
