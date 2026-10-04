//! Worker-side `CtrlRequest` (one outbound `select!` writer) and `Negotiated` (handshake snapshot for [`NativeClient`]).

use crate::config::{CompositorPref, GamepadPref, Mode};
use crate::quic::{
    ClipControl, ClipOffer, ColorInfo, DeliveryReport, LossReport, ProbeRequest, RfiRequest,
};

/// One outbound enum so the worker's `select!` has a single writer — two `&mut ctrl_send`
/// borrows across branches do not compile.
pub(crate) enum CtrlRequest {
    Mode(Mode),
    Probe(ProbeRequest),
    /// Toward a host that answered the delivery tag; see [`crate::quic::ProbeShaped`].
    ProbeShaped(crate::quic::ProbeShaped),
    /// Toward a host that answered the delivery tag; see [`crate::quic::SetDelivery`].
    SetDelivery(u8),
    Keyframe,
    /// Client saw a `frame_index` gap; an RFI-capable host re-references a known-good picture
    /// instead of a full IDR.
    Rfi(RfiRequest),
    Loss(LossReport),
    /// Follows every [`CtrlRequest::Loss`]. `loss_ppm` is 0 for both no loss and no packets;
    /// this count is what separates them.
    Delivery(DeliveryReport),
    /// Once, after the bring-up ramp: the rate it proved the link carries (kbps).
    LinkRate(u32),
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
    /// ~1 Hz latch grid in host-clock time (`design/phase-locked-capture.md`). Latest-wins;
    /// old hosts ignore it.
    Phase(crate::quic::PhaseReport),
    /// A key press or release, toward a host that reads them off the control stream
    /// (`HOST_CAP2_INPUT_EDGES`): QUIC resends what the datagram plane would lose.
    InputEdge(crate::input::InputEvent),
}

/// Handshake snapshot the worker reports to [`NativeClient::connect`]. Field-for-field copy onto
/// the public `NativeClient` of the same names.
#[derive(Clone)]
pub(crate) struct Negotiated {
    pub(crate) mode: Mode,
    /// Chunk-aligned parse window for wire shards.
    pub(crate) shard_payload: u16,
    pub(crate) compositor: CompositorPref,
    pub(crate) gamepad: GamepadPref,
    /// SHA-256 of the presented host cert; TOFU callers persist this.
    pub(crate) host_fingerprint: [u8; 32],
    /// `0` = older host.
    pub(crate) bitrate_kbps: u32,
    /// Host clock minus client clock (ns). `0` = no skew handshake (old host or synced clocks).
    pub(crate) clock_offset_ns: i64,
    /// Connect-time min RTT (ns). `None` = host never answered, so mid-stream re-sync stays off.
    /// Seeds [`ResyncGuard`]'s session-floor.
    pub(crate) clock_rtt_ns: Option<u64>,
    /// `8`, or `10` for Main10 / HDR.
    pub(crate) bit_depth: u8,
    pub(crate) color: ColorInfo,
    /// HEVC `chroma_format_idc`: 1 = 4:2:0, 3 = 4:4:4.
    pub(crate) chroma_format: u8,
    /// Channel count the audio decoder must be built from.
    pub(crate) audio_channels: u8,
    /// Selects the decoder. A 48 kHz/16-bit PCM session and a 48 kHz Opus session match on every
    /// other field.
    pub(crate) audio_codec: u8,
    /// Host capture rate (Hz); may be lower than requested. Open the output device from this,
    /// never the request (`design/hi-res-audio.md`).
    pub(crate) audio_rate_hz: u32,
    /// Unpack stride for `0xD3` payloads (16 or 24). A 24-bit payload at 2 bytes/sample is noise,
    /// not silence.
    pub(crate) audio_bits: u8,
    /// `0xD3` frame duration (µs). `0` on Opus (fixed 5 ms on `0xC9`). Negotiated from path MTU,
    /// never assumed.
    pub(crate) audio_frame_us: u16,
    /// `Welcome::audio_layout`, verbatim.
    pub(crate) audio_layout: u8,
    /// The one codec the host will emit (`quic::CODEC_*`).
    pub(crate) codec: u8,
    /// [`crate::quic::Welcome::host_caps`], surfaced as [`NativeClient::host_caps`] so the
    /// embedder can grey out unsupported toggles.
    pub(crate) host_caps: u8,
    /// [`crate::quic::HOST_CAP2_REPEAT_MARK`]: unflagged AUs are new content. `0` from an older host.
    pub(crate) host_caps2: u8,
    /// Host management-API port; `0` if not advertised. Lets a client reach the game library
    /// without an mDNS advert.
    pub(crate) mgmt_port: u16,
    /// Connect-time grants. An old host decodes to [`crate::quic::GRANT_ALL`]. Starting mask
    /// only — [`crate::quic::AccessUpdate`] moves the live one on the control task
    /// (see [`crate::NativeClient::access_grants`]).
    pub(crate) grants: u32,
    /// Seconds until access expires; `0` = permanent. Connect-time seed for the live deadline,
    /// same as `grants`.
    pub(crate) expires_in_secs: u32,
    /// The profile the host resolved this session to; `None` from a host without profiles.
    pub(crate) profile: Option<String>,
}
