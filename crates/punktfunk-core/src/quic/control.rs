//! The control stream's messages after the handshake. Their wire form is [`super::v2::msg`]; a
//! peer skips a frame type it does not know.
//!
//! Clipboard: `design/clipboard-and-file-transfer.md`. Shard grow/shrink:
//! `design/shard-payload-reneg.md`.

#[cfg(doc)]
use super::{clock_offset_ns, Hello};
use crate::config::Mode;

/// `client → host`: switch display mode without reconnecting.
/// Host answers [`Reconfigured`]. On accept it rebuilds output + encoder; the
/// data plane is unchanged. The first new-mode frame is an IDR with in-band
/// parameter sets — that is all a decoder needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reconfigure {
    pub mode: Mode,
}

/// `host → client` answer to [`Reconfigure`]. `accepted = false`: request
/// rejected (encoder limits); `mode` is the still-active one. `true`: `mode`
/// is being switched to live.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reconfigured {
    pub accepted: bool,
    pub mode: Mode,
}

/// `host → client`: sealed shard payload changes mid-session
/// (`design/shard-payload-reneg.md`). Only to a client whose
/// [`Hello::max_shard_payload`] advertised per-frame geometry, never above that
/// ceiling. Shrink: packetizer may re-key at the next AU; [`ShardPayloadAck`] is
/// telemetry. Grow: no sealed datagram above the OLD size until the ack — the
/// ack is the gate. No `effective_frame_index`: each packet carries `shard_bytes`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShardPayloadChanged {
    /// Even, within the client's advertised bounds.
    pub shard_payload: u16,
}

/// `client → host` answer to [`ShardPayloadChanged`]. Out-of-bounds is dropped
/// with no ack — silence must not read as a granted grow. Echoed value is the
/// grant for a pending grow.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShardPayloadAck {
    pub shard_payload: u16,
}

/// What the host knows about its end of the path, from its `StreamConfig`. `0` means the OS
/// did not say, never "none".
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HostLink {
    /// `0` unknown, `1` Ethernet, `2` Wi-Fi, `3` other.
    pub iface_kind: u8,
    pub link_mbps: u32,
    /// The data socket's granted send buffer.
    pub sndbuf_kb: u32,
    /// What the host's operator pinned ([`StreamConfig::host_forced_shape`](crate::quic::v2::msg::StreamConfig::host_forced_shape)).
    pub forced_shape: u8,
}

impl HostLink {
    pub fn of(cfg: &crate::quic::v2::msg::StreamConfig) -> HostLink {
        HostLink {
            iface_kind: cfg.host_iface_kind,
            link_mbps: cfg.host_link_mbps,
            sndbuf_kb: cfg.host_sndbuf_kb,
            forced_shape: cfg.host_forced_shape,
        }
    }

    /// The host's port as [`LinkFacts`](crate::quic::LinkFacts).
    pub fn facts(&self) -> crate::quic::LinkFacts {
        crate::quic::LinkFacts {
            kind: self.iface_kind,
            mbps: self.link_mbps,
        }
    }
}

pub use crate::transport::{
    IFACE_KIND_ETHERNET, IFACE_KIND_OTHER, IFACE_KIND_UNKNOWN, IFACE_KIND_WIFI,
};

/// `client → host`: retarget encoder bitrate without
/// reconnecting. Host clamps like [`Hello::bitrate_kbps`] (`0` → default),
/// answers [`BitrateChanged`], and retargets in place. Automatic-bitrate
/// clients send this; silence (unknown type) disables the controller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SetBitrate {
    pub bitrate_kbps: u32,
}

/// Why an ack is short of what was asked. The tenth byte of
/// [`BitrateChanged`], toward a client whose `Start` carried
/// [`EXT_TAG_ABR`](super::EXT_TAG_ABR) with
/// [`EXT_ABR_ACK_REASON`](super::EXT_ABR_ACK_REASON) set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AckReason {
    /// Nothing on the host held the rate down. A request past the host's
    /// absolute rate bound still reads as granted: that constant bounds the
    /// absurd, not the link.
    Granted,
    /// The driver or the codec level applied less than the ask.
    EncoderLimit,
    /// Encode is behind the frame cadence, so the climb is refused for now.
    /// A GPU fact, not a rate the encoder cannot hold.
    Cadence,
    /// The host divided a shared path between sessions. Reserved: no host
    /// sends it yet.
    Governor,
    /// PyroWave at an explicit rate: the pin is not negotiable.
    Pinned,
}

impl AckReason {
    pub(crate) fn to_wire(self) -> u8 {
        match self {
            AckReason::Granted => 0,
            AckReason::EncoderLimit => 1,
            AckReason::Cadence => 2,
            AckReason::Governor => 3,
            AckReason::Pinned => 4,
        }
    }

    /// `None` for a code this build does not know: the ack still stands, and
    /// the client falls back to reading a short one as an encoder limit.
    pub(crate) fn from_wire(b: u8) -> Option<AckReason> {
        Some(match b {
            0 => AckReason::Granted,
            1 => AckReason::EncoderLimit,
            2 => AckReason::Cadence,
            3 => AckReason::Governor,
            4 => AckReason::Pinned,
            _ => return None,
        })
    }
}

/// `host → client` answer to [`SetBitrate`]: the clamped configured rate.
/// In-place retarget has no IDR; a rebuild switches on the next frame (IDR).
/// No answer ⇒ an old host that does not renegotiate bitrate.
///
/// Nine bytes, or ten with [`AckReason`]. The host lengthens it only for a
/// client whose `Start` carried [`EXT_TAG_ABR`](super::EXT_TAG_ABR) with
/// [`EXT_ABR_ACK_REASON`](super::EXT_ABR_ACK_REASON) set, because every other
/// client rejects an ack of any other length. `reason: None` is what an older
/// host sends and what this host sends to a client that did not ask.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BitrateChanged {
    pub bitrate_kbps: u32,
    pub reason: Option<AckReason>,
}

/// `host → client`, unsolicited: capture+encoder rebuilt in place; nothing
/// flowed for `gap_ms`. Host-local — no packet lost — but a 750 ms ABR
/// window straddling the rebuild looks like congestion.
///
/// A duration, never an instant: host and client clocks are not one domain.
/// The client anchors the gap to its own receive time; `gap_ms` is log
/// evidence, not an input to the arithmetic.
///
/// Fire-and-forget, and only after a rebuild that succeeded. Failure of
/// eviction recovery ends the session (reconnect re-baselines). A failed
/// mode-switch keeps the old mode and does not announce that stall.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PipelineGap {
    pub gap_ms: u32,
}

/// `client → host`: bandwidth probe. Host bursts
/// [`crate::packet::FLAG_PROBE`] AUs at `target_kbps` for `duration_ms`
/// beside the video it is already sending, then replies [`ProbeResult`].
/// So the reading is headroom, not an idle link. Host clamps both fields.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProbeRequest {
    pub target_kbps: u32,
    pub duration_ms: u32,
}

/// `host → client`: probe burst finished. Splits host-side drops (send
/// buffer; raise `net.core.wmem_max`) from link loss. Client computes:
///
/// - link loss  = `(wire_packets_sent − received) / wire_packets_sent`
/// - host drop  = `send_dropped / (wire_packets_sent + send_dropped)`
/// - throughput = `received_wire_bytes * 8 / duration_ms`
///
/// Packet-level (not reassembled AUs) so the figure degrades past FEC
/// instead of dropping to zero at the budget.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProbeResult {
    pub bytes_sent: u64,
    pub packets_sent: u32,
    pub duration_ms: u32,
    /// Packets the kernel accepted. `0` from a pre-wire-stats host.
    pub wire_packets_sent: u32,
    /// Packets not handed to the kernel (send buffer full).
    pub send_dropped: u32,
}

/// `client → host`: a [`ProbeRequest`] with a shape. `burst_hz` `0` is the smooth train a
/// plain request sends; otherwise every `1/burst_hz` the host releases `rate/burst_hz`
/// bytes in `group_bytes` groups on a `group_rate_kbps` clock (`0` = line rate) — what
/// video does, or what a paced clock would. Same clamps and spacing as the plain
/// request, answered by the same [`ProbeResult`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProbeShaped {
    pub target_kbps: u32,
    pub duration_ms: u32,
    pub burst_hz: u16,
    pub group_bytes: u32,
    pub group_rate_kbps: u32,
}

impl From<ProbeRequest> for ProbeShaped {
    /// The smooth train, unshaped.
    fn from(r: ProbeRequest) -> ProbeShaped {
        ProbeShaped {
            target_kbps: r.target_kbps,
            duration_ms: r.duration_ms,
            burst_hz: 0,
            group_bytes: 0,
            group_rate_kbps: 0,
        }
    }
}

/// `client → host`: one round of the wall-clock skew
/// handshake. Client stamps `t1_ns`; host answers [`ClockEcho`]. A few
/// rounds estimate host−client offset so AU `pts_ns` latency is meaningful
/// across machines. An old host ignores it; the client times out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClockProbe {
    pub t1_ns: u64,
}

/// `host → client` answer to [`ClockProbe`]. `t2_ns`/`t3_ns` are host
/// receive/send; `t1_ns` is echoed. With client receive `t4`:
/// offset = ((t2−t1)+(t3−t4))/2, RTT = (t4−t1)−(t3−t2). See [`clock_offset_ns`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClockEcho {
    pub t1_ns: u64,
    pub t2_ns: u64,
    pub t3_ns: u64,
}

/// A window's `loss_ppm` ([`v2::dgram::Feedback`](super::v2::dgram::Feedback)) from its
/// session-stat deltas: the
/// shard loss parity repaired, and nothing else.
///
/// Loss ≈ (recovered − late) / (received + recovered − late): late shards
/// reconstructed early then arrived, so they are reorder not loss (netted
/// from both ends; saturating so a straddling window cannot go negative).
/// Frames parity could not repair are not in here — the host reads those
/// off the RFI asks they raise. Capped at 1e6.
pub fn window_loss_ppm(recovered: u64, late: u64, received: u64) -> u32 {
    let lost = recovered.saturating_sub(late);
    let denom = received.saturating_add(lost);
    let ppm = lost
        .saturating_mul(1_000_000)
        .checked_div(denom)
        .unwrap_or(0) as u32;
    ppm.min(1_000_000)
}

// Shared clipboard & file transfer (`design/clipboard-and-file-transfer.md`).
// Control, state and offer ride the control stream; fetch and its header ride a
// per-transfer stream, never dispatched by control loops.

/// Absent ⇒ files are filtered from offers in both directions.
pub const CLIP_FLAG_FILES: u8 = 0x01;

/// Always set while enabled unless a future direction limit clears it.
pub const CLIP_POLICY_TEXT: u8 = 0x01;
/// Cleared by operator `no-files` / `text-only`.
pub const CLIP_POLICY_FILES: u8 = 0x02;

pub const CLIP_REASON_OK: u8 = 0;
/// No working clipboard backend for this session type.
pub const CLIP_REASON_BACKEND_UNAVAILABLE: u8 = 1;
/// Another client took the single per-desktop clipboard binding.
pub const CLIP_REASON_TAKEN_OVER: u8 = 2;
pub const CLIP_REASON_POLICY_DISABLED: u8 = 3;
/// Enabled, but host policy forbids file transfer.
pub const CLIP_REASON_NO_FILES: u8 = 4;
/// Distinct from [`CLIP_REASON_POLICY_DISABLED`]: host allows clipboard,
/// this device's grants do not (`GRANT_CLIPBOARD`).
pub const CLIP_REASON_NOT_PERMITTED: u8 = 5;

/// Data chunks follow until FIN.
pub const CLIP_FETCH_OK: u8 = 0;
/// `seq` is no longer current. Paste nothing rather than wrong data. No chunks.
pub const CLIP_FETCH_STALE: u8 = 1;
/// Format/index not available. No chunks.
pub const CLIP_FETCH_UNAVAILABLE: u8 = 2;
/// Policy/cap denies this fetch. No chunks.
pub const CLIP_FETCH_DENIED: u8 = 3;

pub const CLIP_MAX_KINDS: usize = 16;
pub const CLIP_MAX_MIME: usize = 128;
/// Not a file fetch (a whole non-file format, or the file manifest).
pub const CLIP_FILE_INDEX_NONE: u32 = u32::MAX;

/// One advertised clipboard format. Bytes never ride here — they cross on a
/// fetch stream only when the destination pastes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClipKind {
    /// Portable wire MIME. ≤ [`CLIP_MAX_MIME`] bytes; longer is rejected on decode.
    pub mime: String,
    /// Best-effort size in bytes; `0` = unknown (streaming provider).
    pub size_hint: u64,
}

/// `client → host` ([`MSG_CLIP_CONTROL`]): flip shared clipboard for this
/// session. Nothing clipboard-related happens until `enabled: true` arrives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClipControl {
    pub enabled: bool,
    /// [`CLIP_FLAG_FILES`] plus reserved bits for future direction limits.
    pub flags: u8,
}

/// `host → client` ([`MSG_CLIP_STATE`]): ack a [`ClipControl`] and push
/// unsolicited policy/backend updates. Client surfaces `reason`/`policy`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClipState {
    pub enabled: bool,
    pub policy: u8,
    pub reason: u8,
}

/// Symmetric ([`MSG_CLIP_OFFER`]): format list only. A new offer replaces the
/// previous; `seq` lets the holder reject stale fetches. Files are one
/// `application/x-punktfunk-files` kind — the list is fetched, never inlined.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClipOffer {
    /// Monotonic per sender; newest wins.
    pub seq: u32,
    pub kinds: Vec<ClipKind>,
}

/// `requester → holder` ([`MSG_CLIP_FETCH`], fetch stream only): first message
/// on a per-transfer bi-stream, naming which format of `seq` to pull.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClipFetch {
    /// Holder answers [`CLIP_FETCH_STALE`] if this is no longer current.
    pub seq: u32,
    /// File index, or [`CLIP_FILE_INDEX_NONE`] for a non-file format / the manifest.
    pub file_index: u32,
    pub mime: String,
}

/// `holder → requester` ([`MSG_CLIP_FETCH_HDR`], fetch stream only). When
/// `status` is not [`CLIP_FETCH_OK`], no chunks follow.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClipFetchHdr {
    pub status: u8,
    /// Bytes that will follow; `0` = unknown (streaming — FIN ends it).
    pub total_size: u64,
}

// Cursor channel (`design/remote-desktop-sweep.md`). Shape rides the control
// stream; per-frame position rides lossy `0xD0` ([`super::datagram::CursorState`]).
// Active only when [`CLIENT_CAP_CURSOR`](super::caps::CLIENT_CAP_CURSOR) met
// [`HOST_CAP_CURSOR`](super::caps::HOST_CAP_CURSOR) — host then stops compositing.

/// Per-side pixel cap. A 0.43 client re-frames a shape behind a `u16` length,
/// so 120² RGBA (57.6 KiB) is the largest it takes. Host downscales anything larger.
pub const CURSOR_SHAPE_MAX_SIDE: u16 = 120;

/// `host → client`: pointer bitmap changed. Never
/// per-frame — [`super::datagram::CursorState`] carries motion. Client caches
/// by `serial`; a known serial is a 14-byte datagram, not a resend.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CursorShape {
    /// Bumped only on shape change; position moves keep it stable.
    pub serial: u32,
    pub w: u16,
    pub h: u16,
    pub hot_x: u16,
    pub hot_y: u16,
    /// Straight-alpha RGBA8, exactly `w * h * 4` bytes, no padding.
    pub rgba: Vec<u8>,
}

/// `client → host` ([`MSG_CURSOR_RENDER`]): who draws the pointer, live.
/// `client_draws: true` = desktop model: host excludes the pointer and
/// forwards [`CursorShape`]/`0xD0`. `false` = capture model: host composites
/// (DWM / encoder blend, including XOR inversion) and the forwarder goes
/// quiet. Cap-negotiated sessions start `true` until told otherwise.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CursorRenderMode {
    pub client_draws: bool,
}

// Per-client access (`design/per-client-access.md`). Grant vocabulary lives
// in [`super::access`]; this is the one host → client control message.

/// `host → client`: grants or remaining lifetime
/// changed. Latest-wins, best-effort — the host enforces regardless. Lets
/// the client re-gate capture and warn before
/// [`ACCESS_EXPIRED_CLOSE_CODE`](crate::reject). Older clients miss the courtesy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AccessUpdate {
    /// [`super::GRANT_GAMEPAD`] family — same vocabulary as [`Welcome`](super::Welcome).
    pub grants: u32,
    /// Seconds until expiry; `0` = permanent.
    pub remaining_secs: u32,
}

/// `host → client` ([`MSG_AUDIO_STATE`]): the operator muted this session from the
/// console. The host stops sending audio datagrams; nothing on the client's side is
/// broken, so the client says so instead of concealing a gap. Latest-wins,
/// best-effort — older clients just go quiet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioState {
    pub muted: bool,
}

/// `host → client` ([`MSG_PAD_SLOTS`]): the OS pad slots this session holds, one
/// bit per slot. Slot `n` is player `n + 1` to a local co-op game, so the client
/// can say which player it is instead of leaving that to whoever moved a stick
/// first. Latest-wins, best-effort; older clients just show nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PadSlots {
    /// Bit `n` set = this session holds OS pad slot `n`. `0` = no pad.
    pub slots: u16,
}

/// `client → host` ([`MSG_PAD_IDENTITY`](super::v2::registry::MSG_PAD_IDENTITY)): who a captured
/// Steam Controller 2 is — its USB serial and its replies to Steam's feature queries — so the
/// host's virtual pad answers as the physical one. Sent before the pad's arrival.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PadIdentity {
    pub pad: u8,
    /// The USB serial string: the pad's engraved serial, or its Puck's.
    pub serial: String,
    /// `(request, reply)` pairs, id first ([`pack_identity_replies`]).
    pub replies: Vec<u8>,
    /// A Puck pad's slot, 0–3: its USB interface less 2. The host seats pads that share a Puck
    /// serial on one virtual Puck at these slots. Zero for a pad on a cable or Bluetooth.
    pub slot: u8,
}

/// `host → client` ([`MSG_PAD_FEATURE`](super::v2::registry::MSG_PAD_FEATURE)): a feature
/// report for the physical pad, id first — [`HidOutput::HidRaw`](super::HidOutput::HidRaw) with
/// [`HID_RAW_FEATURE`](super::HID_RAW_FEATURE), on the control stream.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PadFeature {
    pub pad: u8,
    pub data: Vec<u8>,
}

/// Longest [`PadIdentity::serial`] in bytes.
pub const PAD_IDENTITY_SERIAL_MAX: usize = 32;
/// Longest [`PadIdentity::replies`] in bytes: room for every query with margin.
pub const PAD_IDENTITY_REPLIES_MAX: usize = 4096;

/// Pack `(request, reply)` pairs as `[len][request][len][reply]…`, each part cut at 64 bytes.
pub fn pack_identity_replies<'a>(pairs: impl IntoIterator<Item = (&'a [u8], &'a [u8])>) -> Vec<u8> {
    let mut out = Vec::new();
    for (req, reply) in pairs {
        for part in [req, reply] {
            let part = &part[..part.len().min(64)];
            out.push(part.len() as u8);
            out.extend_from_slice(part);
        }
    }
    out
}

/// The pairs [`pack_identity_replies`] packed; `None` when a length runs past the end.
pub fn unpack_identity_replies(b: &[u8]) -> Option<Vec<(Vec<u8>, Vec<u8>)>> {
    let mut parts = Vec::new();
    let mut rest = b;
    while let Some((&n, tail)) = rest.split_first() {
        let n = usize::from(n);
        if n > 64 || tail.len() < n {
            return None;
        }
        parts.push(tail[..n].to_vec());
        rest = &tail[n..];
    }
    if parts.len() % 2 != 0 {
        return None;
    }
    let mut it = parts.into_iter();
    let mut pairs = Vec::new();
    while let (Some(req), Some(reply)) = (it.next(), it.next()) {
        pairs.push((req, reply));
    }
    Some(pairs)
}

/// Longest [`LaunchOutcome::message`] in UTF-8 bytes. One sentence plus a cause;
/// a host cannot make the client hold more than this.
pub const LAUNCH_MESSAGE_MAX: usize = 200;

/// How a session's library launch turned out.
///
/// Wire values are append-only: an unknown byte decodes to [`Self::Spawned`], the
/// one reading under which an older client keeps waiting instead of raising an
/// alarm it cannot justify. The host's own vocabulary maps onto this one — no
/// second set of names to drift.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LaunchOutcomeKind {
    /// The host started the title for this session.
    #[default]
    Spawned = 0,
    /// Not started: an earlier session's copy is verified up, and this session
    /// took that one over.
    Adopted = 1,
    /// Adopted, but the host cannot see the process — it may be up, it may be
    /// gone. The player is owed the "start it again" move.
    AdoptedUnknown = 2,
    /// The host declined before trying: unknown id, no command, nothing to run.
    Refused = 3,
    /// Started and gone within seconds, with nothing left that looks like the game.
    Failed = 4,
    /// Handed to a Steam that has no account: the stream shows its sign-in
    /// screen, and the player plays once they are through it.
    SignInNeeded = 5,
}

impl LaunchOutcomeKind {
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Adopted,
            2 => Self::AdoptedUnknown,
            3 => Self::Refused,
            4 => Self::Failed,
            5 => Self::SignInNeeded,
            _ => Self::Spawned,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Spawned => "spawned",
            Self::Adopted => "adopted",
            Self::AdoptedUnknown => "adopted-unknown",
            Self::Refused => "refused",
            Self::Failed => "failed",
            Self::SignInNeeded => "sign-in-needed",
        }
    }

    /// Did the player get the game they asked for? `false` is what a client
    /// turns into a message; the rest needs no words.
    pub fn needs_telling(self) -> bool {
        matches!(
            self,
            Self::AdoptedUnknown | Self::Refused | Self::Failed | Self::SignInNeeded
        )
    }
}

/// `host → client` ([`MSG_LAUNCH_OUTCOME`]): what became of this session's launch.
///
/// Sent once the launch resolves, and again if the game dies on the spot — so it
/// is a control message, not a `Welcome` field: the answer is not known while the
/// handshake runs. `message` is the host's own sentence, empty when it has none.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaunchOutcome {
    pub kind: LaunchOutcomeKind,
    /// Plain sentence for the player, already stripped and capped by [`Self::new`].
    pub message: String,
}

impl LaunchOutcome {
    /// Trims, drops control characters, and truncates to [`LAUNCH_MESSAGE_MAX`] on a
    /// char boundary. Empty in means empty out, which is "say nothing".
    pub fn new(kind: LaunchOutcomeKind, message: &str) -> Self {
        let mut message: String = message.trim().chars().filter(|c| !c.is_control()).collect();
        while message.len() > LAUNCH_MESSAGE_MAX {
            message.pop();
        }
        LaunchOutcome { kind, message }
    }

    /// The line a client shows: the host's sentence, when the player did not get the game
    /// they asked for. `None` when the launch needs no words.
    pub fn notice(&self) -> Option<&str> {
        (self.kind.needs_telling() && !self.message.is_empty()).then_some(self.message.as_str())
    }
}

/// `client → host` ([`MSG_INPUT_EDGE`]): one input event whose loss would stick — a key
/// press or release — on the control stream instead of the datagram plane, so QUIC resends
/// it and keeps its order. Sent only toward [`HOST_CAP2_INPUT_EDGES`](super::HOST_CAP2_INPUT_EDGES).
/// The payload is the datagram encoding unchanged, so the host feeds both paths into one
/// queue; any kind decodes, and the grants gate it exactly as they gate the datagram.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InputEdge(pub crate::input::InputEvent);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quic::v2::msg::V2Message;

    #[test]
    fn window_loss_ppm_estimates_and_caps() {
        assert_eq!(window_loss_ppm(0, 0, 0), 0);
        assert_eq!(window_loss_ppm(0, 0, 1000), 0);
        // 50 of 1000 = 5%.
        assert_eq!(window_loss_ppm(50, 0, 950), 50_000);
        assert!(window_loss_ppm(u64::MAX, 0, 1) <= 1_000_000);
        // Late shards are reorder, not loss. 20 of 1000 = 2%.
        assert_eq!(window_loss_ppm(50, 50, 1000), 0);
        assert_eq!(window_loss_ppm(50, 30, 980), 20_000);
        // `late` can outrun `recovered` across a window boundary — saturate, never underflow.
        assert_eq!(window_loss_ppm(10, 25, 1000), 0);
    }

    /// The message is capped and stripped where it is built, not where it is shown:
    /// a control character from a game's own output must never reach a terminal.
    #[test]
    fn launch_outcome_message_is_bounded_and_plain() {
        let long = LaunchOutcome::new(LaunchOutcomeKind::Failed, &"\u{e9}".repeat(400));
        assert!(long.message.len() <= LAUNCH_MESSAGE_MAX);
        assert_eq!(
            LaunchOutcome::from_body(&long.fields().into_body()).unwrap(),
            long
        );
        assert_eq!(
            LaunchOutcome::new(LaunchOutcomeKind::Refused, "  a\u{7}b\n  ").message,
            "ab"
        );
    }
}
