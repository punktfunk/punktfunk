//! Worker-side `CtrlRequest` (one outbound `select!` writer) and `Negotiated` (handshake snapshot for [`NativeClient`]).

use crate::config::Mode;
use crate::quic::{ClipControl, ClipOffer, ProbeRequest, Welcome};

/// One outbound enum so the worker's `select!` has a single writer — two `&mut ctrl_send`
/// borrows across branches do not compile.
pub(crate) enum CtrlRequest {
    /// A mode and the view the picture is shown at, `None` for the mode itself.
    Mode(Mode, Option<(u32, u32)>),
    /// A codec ask: these `CODEC_*` bits can't be decoded here.
    Codec(u8),
    Probe(ProbeRequest),
    ProbeShaped(crate::quic::ProbeShaped),
    /// The pump's [`BitrateController`] sends this (kbps) when bitrate is Automatic.
    SetBitrate(u32),
    /// Pump sends this after the first no-op clock flush; the control task also fires one every
    /// [`CLOCK_RESYNC_INTERVAL`].
    ClockResync,
    /// Idempotent. File-permission flag is in the payload (`design/clipboard-and-file-transfer.md`).
    ClipControl(ClipControl),
    /// Lazy format-list only; bytes follow on a fetch stream. The host may send one too.
    ClipOffer(ClipOffer),
    /// Who draws the pointer. Client-local = host excludes and forwards; host-composite = baked
    /// into the video. Latest-wins.
    CursorRender(crate::quic::CursorRenderMode),
    /// A key or button press or release, or a character of text, toward a host that reads them
    /// off the control stream (`HOST_CAP2_INPUT_EDGES`): QUIC resends what the datagram plane
    /// would lose and keeps their order.
    InputEdge(crate::input::InputEvent),
    /// A captured Steam Controller 2's identity, ahead of its arrival.
    PadIdentity(crate::quic::PadIdentity),
    /// The screens the embedder shows now, screen 0 first.
    Screens(Vec<crate::quic::v2::msg::Screen>),
}

/// Handshake snapshot the worker reports to [`NativeClient::connect`]: the host's offer as sent,
/// plus what the dial itself learned.
#[derive(Clone)]
pub(crate) struct Negotiated {
    pub(crate) welcome: Welcome,
    /// SHA-256 of the presented host cert; TOFU callers persist this.
    pub(crate) host_fingerprint: [u8; 32],
    /// Host clock minus client clock (ns). `0` = no skew handshake (old host or synced clocks).
    pub(crate) clock_offset_ns: i64,
    /// Connect-time min RTT (ns). `None` = host never answered, so mid-stream re-sync stays off.
    /// Seeds [`ResyncGuard`]'s session-floor.
    pub(crate) clock_rtt_ns: Option<u64>,
    /// The profile the host resolved this session to; `None` from a host without profiles.
    pub(crate) profile: Option<String>,
    /// The screen the host says this session streams; `None` from a host that does not say.
    pub(crate) screen: Option<crate::quic::v2::hello::ScreenEcho>,
    /// The role the host says this session got; `None` from a host that does not say.
    pub(crate) role: Option<crate::quic::v2::hello::RoleEcho>,
    /// The host counts axis-unit scroll (`FEATURE_SCROLL_AXIS_UNITS`).
    pub(crate) scroll_axis_units: bool,
    /// The size the host encodes the Welcome's mode at; `None` encodes the mode.
    pub(crate) picture: Option<(u32, u32)>,
}
