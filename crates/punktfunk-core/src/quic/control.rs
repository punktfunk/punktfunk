//! Typed post-handshake control messages (`CTL_MAGIC` + type byte).
//!
//! Handshake is positional and stays untyped for wire compatibility. After
//! [`Start`], every message is `magic || type || payload`. Unknown types are
//! ignored, so mixed versions stay forward-safe: a new message never
//! lengthens an old one ([`LossReport`] decode is exact-length).
//!
//! Encode layout is the `// magic[0..4] type[4] …` comment on each `encode`.
//! Clipboard: `design/clipboard-and-file-transfer.md`. Shard grow/shrink:
//! `design/shard-payload-reneg.md`. Phase lock: `design/phase-locked-capture.md`.

use super::wire::{Rd, Wr};
#[cfg(doc)]
use super::{clock_offset_ns, Hello, Start};
use crate::config::Mode;
use crate::error::{PunktfunkError, Result};

/// `client → host` after [`Start`]: switch display mode without reconnecting.
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

/// `client → host` after [`Start`]: force the next frame to an IDR with
/// in-band parameter sets. Infinite GOP is one opening IDR then P-frames, so
/// a wedged decoder stays frozen until the next loss-triggered keyframe.
/// Fire-and-forget — the recovered IDR is the ack.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RequestKeyframe;

/// `client → host`: invalidate `[first_frame, last_frame]` instead of a full
/// IDR (a 20–40× spike). A host that can RFI re-references a picture before
/// `first_frame` and tags the P-frame [`crate::packet::USER_FLAG_RECOVERY_ANCHOR`].
/// Else it forces an IDR, as for [`RequestKeyframe`]. Fire-and-forget.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RfiRequest {
    pub first_frame: u32,
    pub last_frame: u32,
}

/// `host → client` after [`Start`]: sealed shard payload changes mid-session
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

/// `client → host`, periodic: observed data-plane loss so the host can size
/// FEC to the link. `loss_ppm` is parts-per-million of shards missing-but-
/// recovered (plus a bump when frames went unrecoverable). Fire-and-forget.
/// An older host ignores the unknown type and keeps static FEC.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LossReport {
    pub loss_ppm: u32,
}

/// `client → host`, right after each [`LossReport`]: cumulative data-plane
/// packets received this session.
///
/// `loss_ppm` is a ratio over arrived packets, so a silent client and a
/// flawless client both report 0. `packets_received == 0` while the host has
/// sent frames is the unambiguous "video is not reaching me" (the control
/// plane carrying this is healthy).
///
/// Own type byte, not a field on [`LossReport`]: that decode is exact-length,
/// so lengthening it would make every shipped host reject adaptive FEC.
/// Cumulative `u64` so one message is self-contained with no saturation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeliveryReport {
    pub packets_received: u64,
}

/// `client → host`, once, after the bring-up ramp: the rate the ramp proved
/// the link carries, kbps. The host paces a pinned stream against it instead
/// of a multiple of the stream rate. Fire-and-forget; an older host ignores
/// the unknown type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LinkReport {
    pub proven_kbps: u32,
}

/// `client → host` mid-session: stream under this delivery profile (`0` burst, `1` capped,
/// `2` smooth) from the next frame. Answered by [`DeliveryChanged`]. Sent only after the
/// host answered the `Start` tag ([`EXT_TAG_DELIVERY`](super::EXT_TAG_DELIVERY)): an older
/// host logs every type it does not know.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SetDelivery {
    pub profile: u8,
}

/// `host → client`: the profile this session now streams under — once after a `Start` that
/// carried the tag, and after every [`SetDelivery`]. `forced` = the host pins one for every
/// session (`PUNKTFUNK_DELIVERY`) and the ask changed nothing. Its arrival is what tells the
/// client the host reads delivery messages at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeliveryChanged {
    pub profile: u8,
    pub forced: bool,
}

/// `host → client`, once after `Start`, toward a tag that set
/// [`EXT_DELIVERY_FACTS`](super::EXT_DELIVERY_FACTS): what the host knows about its own end
/// of the path. `link_mbps` `0` and `iface_kind` `0` mean the OS did not say, never "none".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostFacts {
    /// `0` unknown, `1` Ethernet, `2` Wi-Fi, `3` other.
    pub iface_kind: u8,
    pub link_mbps: u32,
    /// The data socket's granted send buffer.
    pub sndbuf_kb: u32,
    /// `PUNKTFUNK_DELIVERY` as a profile byte, `0xFF` when unset.
    pub forced_profile: u8,
}

pub use crate::transport::{
    IFACE_KIND_ETHERNET, IFACE_KIND_OTHER, IFACE_KIND_UNKNOWN, IFACE_KIND_WIFI,
};
/// [`HostFacts::forced_profile`] when nothing is pinned.
pub const FORCED_PROFILE_NONE: u8 = 0xFF;

/// `client → host` after [`Start`]: retarget encoder bitrate without
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
    /// PyroWave: the rate is per-frame CBR and not negotiable.
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

/// `client → host` after [`Start`]: bandwidth probe. Host bursts
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
/// video does, or what a capped profile would. Same clamps and spacing as the plain
/// request, answered by the same [`ProbeResult`]. Sent only toward a host that answered
/// the delivery tag.
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

/// `client → host` after [`Start`]: one round of the wall-clock skew
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

/// `client → host`, ~1 Hz: display-latch grid so the host can phase-lock
/// capture (`design/phase-locked-capture.md`). Gated on
/// [`CLIENT_CAP_PHASE_LOCK`](crate::quic::CLIENT_CAP_PHASE_LOCK).
///
/// Timestamps are host `CLOCK_REALTIME`: the client converts before send
/// (`T_host = T_client + offset`). The offset lives only client-side.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PhaseReport {
    /// Next display latch, host clock. Host extrapolates by `latch_period_ns`.
    pub next_latch_host_ns: u64,
    /// Panel refresh period (true latch grid, not a down-rated callback).
    pub latch_period_ns: u32,
    /// Skew residual + latch jitter p95. Host widens its margin by this,
    /// never narrows below its floor.
    pub uncertainty_ns: u32,
    /// Arrival-before-latch lead, ns, clamped ≥ 0. v2: circular mean mod
    /// period; v1: window median. Error signal toward the target lead.
    pub arrival_lead_ns: u32,
    /// Arrival-phase coherence, ‰ (0 = smeared, 1000 = locked). [`u16::MAX`]
    /// = v1 25-byte form with no coherence; host uses its travel-cap only.
    pub coherence_milli: u16,
}

pub const MSG_RECONFIGURE: u8 = 0x01;
pub const MSG_RECONFIGURED: u8 = 0x02;
pub const MSG_REQUEST_KEYFRAME: u8 = 0x03;
pub const MSG_LOSS_REPORT: u8 = 0x04;
pub const MSG_SET_BITRATE: u8 = 0x05;
pub const MSG_BITRATE_CHANGED: u8 = 0x06;
pub const MSG_RFI_REQUEST: u8 = 0x07;
pub const MSG_SHARD_PAYLOAD_CHANGED: u8 = 0x08;
pub const MSG_SHARD_PAYLOAD_ACK: u8 = 0x09;
/// [`PipelineGap`]. 0x0A stays in the 0x01–0x09 video/rate-control block
/// (same ABR consumer). Not 0x30: it carries a duration, no clock domain.
pub const MSG_PIPELINE_GAP: u8 = 0x0A;
pub const MSG_DELIVERY_REPORT: u8 = 0x0B;
pub const MSG_LINK_REPORT: u8 = 0x0C;
pub const MSG_SET_DELIVERY: u8 = 0x0D;
pub const MSG_DELIVERY_CHANGED: u8 = 0x0E;
pub const MSG_HOST_FACTS: u8 = 0x0F;
pub const MSG_PROBE_REQUEST: u8 = 0x20;
pub const MSG_PROBE_RESULT: u8 = 0x21;
pub const MSG_PROBE_SHAPED: u8 = 0x22;
pub const MSG_CLOCK_PROBE: u8 = 0x30;
pub const MSG_CLOCK_ECHO: u8 = 0x31;
pub const MSG_PHASE_REPORT: u8 = 0x32;

/// `width u32 ‖ height u32 ‖ refresh_hz u32`, shared by [`Reconfigure`] and [`Reconfigured`].
fn put_mode(w: Wr, m: Mode) -> Wr {
    w.u32(m.width).u32(m.height).u32(m.refresh_hz)
}

fn read_mode(r: &mut Rd) -> Mode {
    Mode {
        width: r.u32(),
        height: r.u32(),
        refresh_hz: r.u32(),
    }
}

impl Reconfigure {
    pub fn encode(&self) -> Vec<u8> {
        // magic[0..4] type[4] w[5..9] h[9..13] hz[13..17]
        put_mode(Wr::ctl(MSG_RECONFIGURE, 17), self.mode).done()
    }

    pub fn decode(b: &[u8]) -> Result<Reconfigure> {
        let mut r = Rd::ctl(b, MSG_RECONFIGURE, 17..=17, "bad Reconfigure")?;
        Ok(Reconfigure {
            mode: read_mode(&mut r),
        })
    }
}

impl Reconfigured {
    pub fn encode(&self) -> Vec<u8> {
        // magic[0..4] type[4] accepted[5] w[6..10] h[10..14] hz[14..18]
        let w = Wr::ctl(MSG_RECONFIGURED, 18).u8(self.accepted as u8);
        put_mode(w, self.mode).done()
    }

    pub fn decode(b: &[u8]) -> Result<Reconfigured> {
        let mut r = Rd::ctl(b, MSG_RECONFIGURED, 18..=18, "bad Reconfigured")?;
        Ok(Reconfigured {
            accepted: r.u8() != 0,
            mode: read_mode(&mut r),
        })
    }
}

impl RequestKeyframe {
    pub fn encode(&self) -> Vec<u8> {
        // magic[0..4] type[4] — no payload
        Wr::ctl(MSG_REQUEST_KEYFRAME, 5).done()
    }

    pub fn decode(b: &[u8]) -> Result<RequestKeyframe> {
        Rd::ctl(b, MSG_REQUEST_KEYFRAME, 5..=5, "bad RequestKeyframe")?;
        Ok(RequestKeyframe)
    }
}

impl RfiRequest {
    pub fn encode(&self) -> Vec<u8> {
        // magic[0..4] type[4] first_frame[5..9] last_frame[9..13]
        Wr::ctl(MSG_RFI_REQUEST, 13)
            .u32(self.first_frame)
            .u32(self.last_frame)
            .done()
    }

    pub fn decode(b: &[u8]) -> Result<RfiRequest> {
        let mut r = Rd::ctl(b, MSG_RFI_REQUEST, 13..=13, "bad RfiRequest")?;
        Ok(RfiRequest {
            first_frame: r.u32(),
            last_frame: r.u32(),
        })
    }
}

impl ShardPayloadChanged {
    pub fn encode(&self) -> Vec<u8> {
        // magic[0..4] type[4] shard_payload[5..7]
        Wr::ctl(MSG_SHARD_PAYLOAD_CHANGED, 7)
            .u16(self.shard_payload)
            .done()
    }

    pub fn decode(b: &[u8]) -> Result<ShardPayloadChanged> {
        let mut r = Rd::ctl(
            b,
            MSG_SHARD_PAYLOAD_CHANGED,
            7..=7,
            "bad ShardPayloadChanged",
        )?;
        Ok(ShardPayloadChanged {
            shard_payload: r.u16(),
        })
    }
}

impl ShardPayloadAck {
    pub fn encode(&self) -> Vec<u8> {
        // magic[0..4] type[4] shard_payload[5..7]
        Wr::ctl(MSG_SHARD_PAYLOAD_ACK, 7)
            .u16(self.shard_payload)
            .done()
    }

    pub fn decode(b: &[u8]) -> Result<ShardPayloadAck> {
        let mut r = Rd::ctl(b, MSG_SHARD_PAYLOAD_ACK, 7..=7, "bad ShardPayloadAck")?;
        Ok(ShardPayloadAck {
            shard_payload: r.u16(),
        })
    }
}

impl LossReport {
    pub fn encode(&self) -> Vec<u8> {
        // magic[0..4] type[4] loss_ppm[5..9]
        Wr::ctl(MSG_LOSS_REPORT, 9).u32(self.loss_ppm).done()
    }

    pub fn decode(b: &[u8]) -> Result<LossReport> {
        let mut r = Rd::ctl(b, MSG_LOSS_REPORT, 9..=9, "bad LossReport")?;
        Ok(LossReport { loss_ppm: r.u32() })
    }
}

impl DeliveryReport {
    pub fn encode(&self) -> Vec<u8> {
        // magic[0..4] type[4] packets_received[5..13]
        Wr::ctl(MSG_DELIVERY_REPORT, 13)
            .u64(self.packets_received)
            .done()
    }

    pub fn decode(b: &[u8]) -> Result<DeliveryReport> {
        let mut r = Rd::ctl(b, MSG_DELIVERY_REPORT, 13..=13, "bad DeliveryReport")?;
        Ok(DeliveryReport {
            packets_received: r.u64(),
        })
    }
}

impl LinkReport {
    pub fn encode(&self) -> Vec<u8> {
        // magic[0..4] type[4] proven_kbps[5..9]
        Wr::ctl(MSG_LINK_REPORT, 9).u32(self.proven_kbps).done()
    }

    pub fn decode(b: &[u8]) -> Result<LinkReport> {
        let mut r = Rd::ctl(b, MSG_LINK_REPORT, 9..=9, "bad LinkReport")?;
        Ok(LinkReport {
            proven_kbps: r.u32(),
        })
    }
}

impl SetDelivery {
    pub fn encode(&self) -> Vec<u8> {
        // magic[0..4] type[4] profile[5]
        Wr::ctl(MSG_SET_DELIVERY, 6).u8(self.profile).done()
    }

    pub fn decode(b: &[u8]) -> Result<SetDelivery> {
        let mut r = Rd::ctl(b, MSG_SET_DELIVERY, 6..=6, "bad SetDelivery")?;
        Ok(SetDelivery { profile: r.u8() })
    }
}

impl DeliveryChanged {
    pub fn encode(&self) -> Vec<u8> {
        // magic[0..4] type[4] profile[5] forced[6]
        Wr::ctl(MSG_DELIVERY_CHANGED, 7)
            .u8(self.profile)
            .u8(self.forced as u8)
            .done()
    }

    pub fn decode(b: &[u8]) -> Result<DeliveryChanged> {
        let mut r = Rd::ctl(b, MSG_DELIVERY_CHANGED, 7..=7, "bad DeliveryChanged")?;
        Ok(DeliveryChanged {
            profile: r.u8(),
            forced: r.u8() != 0,
        })
    }
}

impl HostFacts {
    pub fn encode(&self) -> Vec<u8> {
        // magic[0..4] type[4] iface_kind[5] link_mbps[6..10] sndbuf_kb[10..14] forced[14]
        Wr::ctl(MSG_HOST_FACTS, 15)
            .u8(self.iface_kind)
            .u32(self.link_mbps)
            .u32(self.sndbuf_kb)
            .u8(self.forced_profile)
            .done()
    }

    pub fn decode(b: &[u8]) -> Result<HostFacts> {
        let mut r = Rd::ctl(b, MSG_HOST_FACTS, 15..=15, "bad HostFacts")?;
        Ok(HostFacts {
            iface_kind: r.u8(),
            link_mbps: r.u32(),
            sndbuf_kb: r.u32(),
            forced_profile: r.u8(),
        })
    }
}

impl ProbeShaped {
    pub fn encode(&self) -> Vec<u8> {
        // magic[0..4] type[4] target[5..9] duration[9..13] burst_hz[13..15]
        // group_bytes[15..19] group_rate[19..23]
        Wr::ctl(MSG_PROBE_SHAPED, 23)
            .u32(self.target_kbps)
            .u32(self.duration_ms)
            .u16(self.burst_hz)
            .u32(self.group_bytes)
            .u32(self.group_rate_kbps)
            .done()
    }

    pub fn decode(b: &[u8]) -> Result<ProbeShaped> {
        let mut r = Rd::ctl(b, MSG_PROBE_SHAPED, 23..=23, "bad ProbeShaped")?;
        Ok(ProbeShaped {
            target_kbps: r.u32(),
            duration_ms: r.u32(),
            burst_hz: r.u16(),
            group_bytes: r.u32(),
            group_rate_kbps: r.u32(),
        })
    }
}

impl SetBitrate {
    pub fn encode(&self) -> Vec<u8> {
        // magic[0..4] type[4] bitrate_kbps[5..9]
        Wr::ctl(MSG_SET_BITRATE, 9).u32(self.bitrate_kbps).done()
    }

    pub fn decode(b: &[u8]) -> Result<SetBitrate> {
        let mut r = Rd::ctl(b, MSG_SET_BITRATE, 9..=9, "bad SetBitrate")?;
        Ok(SetBitrate {
            bitrate_kbps: r.u32(),
        })
    }
}

impl BitrateChanged {
    pub fn encode(&self) -> Vec<u8> {
        // magic[0..4] type[4] bitrate_kbps[5..9] reason[9] (optional)
        let w = Wr::ctl(MSG_BITRATE_CHANGED, 10).u32(self.bitrate_kbps);
        match self.reason {
            Some(r) => w.u8(r.to_wire()),
            None => w,
        }
        .done()
    }

    pub fn decode(b: &[u8]) -> Result<BitrateChanged> {
        let mut r = Rd::ctl(b, MSG_BITRATE_CHANGED, 9..=10, "bad BitrateChanged")?;
        Ok(BitrateChanged {
            bitrate_kbps: r.u32(),
            reason: r.opt_u8().and_then(AckReason::from_wire),
        })
    }
}

impl PipelineGap {
    pub fn encode(&self) -> Vec<u8> {
        // magic[0..4] type[4] gap_ms[5..9]
        Wr::ctl(MSG_PIPELINE_GAP, 9).u32(self.gap_ms).done()
    }

    pub fn decode(b: &[u8]) -> Result<PipelineGap> {
        let mut r = Rd::ctl(b, MSG_PIPELINE_GAP, 9..=9, "bad PipelineGap")?;
        Ok(PipelineGap { gap_ms: r.u32() })
    }
}

/// [`LossReport`] `loss_ppm` from one window's session-stat deltas: the
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

impl ProbeRequest {
    pub fn encode(&self) -> Vec<u8> {
        // magic[0..4] type[4] target_kbps[5..9] duration_ms[9..13]
        Wr::ctl(MSG_PROBE_REQUEST, 13)
            .u32(self.target_kbps)
            .u32(self.duration_ms)
            .done()
    }

    pub fn decode(b: &[u8]) -> Result<ProbeRequest> {
        let mut r = Rd::ctl(b, MSG_PROBE_REQUEST, 13..=13, "bad ProbeRequest")?;
        Ok(ProbeRequest {
            target_kbps: r.u32(),
            duration_ms: r.u32(),
        })
    }
}

impl ProbeResult {
    pub fn encode(&self) -> Vec<u8> {
        // magic[0..4] type[4] bytes_sent[5..13] packets_sent[13..17] duration_ms[17..21]
        // wire_packets_sent[21..25] send_dropped[25..29]
        Wr::ctl(MSG_PROBE_RESULT, 29)
            .u64(self.bytes_sent)
            .u32(self.packets_sent)
            .u32(self.duration_ms)
            .u32(self.wire_packets_sent)
            .u32(self.send_dropped)
            .done()
    }

    pub fn decode(b: &[u8]) -> Result<ProbeResult> {
        // 21 bytes = pre-wire-stats host (new fields 0); 29 = with wire stats. Reject shorter.
        let mut r = Rd::ctl(b, MSG_PROBE_RESULT, 21.., "bad ProbeResult")?;
        let (bytes_sent, packets_sent, duration_ms) = (r.u64(), r.u32(), r.u32());
        let (wire_packets_sent, send_dropped) = if r.remaining() >= 8 {
            (r.u32(), r.u32())
        } else {
            (0, 0)
        };
        Ok(ProbeResult {
            bytes_sent,
            packets_sent,
            duration_ms,
            wire_packets_sent,
            send_dropped,
        })
    }
}

impl ClockProbe {
    pub fn encode(&self) -> Vec<u8> {
        // magic[0..4] type[4] t1[5..13]
        Wr::ctl(MSG_CLOCK_PROBE, 13).u64(self.t1_ns).done()
    }

    pub fn decode(b: &[u8]) -> Result<ClockProbe> {
        let mut r = Rd::ctl(b, MSG_CLOCK_PROBE, 13..=13, "bad ClockProbe")?;
        Ok(ClockProbe { t1_ns: r.u64() })
    }
}

impl ClockEcho {
    pub fn encode(&self) -> Vec<u8> {
        // magic[0..4] type[4] t1[5..13] t2[13..21] t3[21..29]
        Wr::ctl(MSG_CLOCK_ECHO, 29)
            .u64(self.t1_ns)
            .u64(self.t2_ns)
            .u64(self.t3_ns)
            .done()
    }

    pub fn decode(b: &[u8]) -> Result<ClockEcho> {
        let mut r = Rd::ctl(b, MSG_CLOCK_ECHO, 29..=29, "bad ClockEcho")?;
        Ok(ClockEcho {
            t1_ns: r.u64(),
            t2_ns: r.u64(),
            t3_ns: r.u64(),
        })
    }
}

impl PhaseReport {
    pub fn encode(&self) -> Vec<u8> {
        // magic[0..4] type[4] latch[5..13] period[13..17] uncertainty[17..21] lead[21..25]
        // coherence[25..27] v2 tail. MAX sentinel encodes as the 25-byte v1 form (append-only).
        let w = Wr::ctl(MSG_PHASE_REPORT, 27)
            .u64(self.next_latch_host_ns)
            .u32(self.latch_period_ns)
            .u32(self.uncertainty_ns)
            .u32(self.arrival_lead_ns);
        match self.coherence_milli {
            u16::MAX => w,
            c => w.u16(c),
        }
        .done()
    }

    pub fn decode(b: &[u8]) -> Result<PhaseReport> {
        const BAD: &str = "bad PhaseReport";
        // 25 bytes is v1, 27 is v2; nothing between.
        if b.len() == 26 {
            return Err(PunktfunkError::InvalidArg(BAD));
        }
        let mut r = Rd::ctl(b, MSG_PHASE_REPORT, 25..=27, BAD)?;
        Ok(PhaseReport {
            next_latch_host_ns: r.u64(),
            latch_period_ns: r.u32(),
            uncertainty_ns: r.u32(),
            arrival_lead_ns: r.u32(),
            // A v1 sender has no coherence signal.
            coherence_milli: r.opt_u16().unwrap_or(u16::MAX),
        })
    }
}

/// Frame a message for the control stream: `u16 LE length || payload`.
pub fn frame(payload: &[u8]) -> Vec<u8> {
    let mut b = Vec::with_capacity(2 + payload.len());
    b.extend_from_slice(&(payload.len() as u16).to_le_bytes());
    b.extend_from_slice(payload);
    b
}

// Shared clipboard & file transfer (`design/clipboard-and-file-transfer.md`).
// 0x40–0x42 ride the control stream; 0x43–0x44 ride a per-transfer bi-stream
// (never dispatched by control loops). Unknown types drop — forward-safe.

/// Idempotent enable/disable. Opt-in is here, not just in UI.
pub const MSG_CLIP_CONTROL: u8 = 0x40;
pub const MSG_CLIP_STATE: u8 = 0x41;
/// Format list only — no clipboard bytes.
pub const MSG_CLIP_OFFER: u8 = 0x42;
/// Fetch stream only — never the control stream.
pub const MSG_CLIP_FETCH: u8 = 0x43;
/// Fetch stream only — header that precedes the data chunks.
pub const MSG_CLIP_FETCH_HDR: u8 = 0x44;

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

/// `mime_len u8 || mime bytes || size_hint u64 LE`.
fn put_clip_kind(w: Wr, k: &ClipKind) -> Wr {
    let mime = k.mime.as_bytes();
    let n = mime.len().min(CLIP_MAX_MIME);
    w.u8(n as u8).bytes(&mime[..n]).u64(k.size_hint)
}

fn get_clip_kind(r: &mut Rd) -> Result<ClipKind> {
    let n = r
        .opt_u8()
        .ok_or(PunktfunkError::InvalidArg("truncated ClipKind"))? as usize;
    if n > CLIP_MAX_MIME {
        return Err(PunktfunkError::InvalidArg("ClipKind mime too long"));
    }
    if r.remaining() < n + 8 {
        return Err(PunktfunkError::InvalidArg("ClipKind overruns message"));
    }
    let mime = String::from_utf8_lossy(r.bytes(n)).into_owned();
    Ok(ClipKind {
        mime,
        size_hint: r.u64(),
    })
}

impl ClipControl {
    pub fn encode(&self) -> Vec<u8> {
        // magic[0..4] type[4] enabled[5] flags[6]
        Wr::ctl(MSG_CLIP_CONTROL, 7)
            .u8(self.enabled as u8)
            .u8(self.flags)
            .done()
    }

    pub fn decode(b: &[u8]) -> Result<ClipControl> {
        let mut r = Rd::ctl(b, MSG_CLIP_CONTROL, 7..=7, "bad ClipControl")?;
        Ok(ClipControl {
            enabled: r.u8() != 0,
            flags: r.u8(),
        })
    }
}

impl ClipState {
    pub fn encode(&self) -> Vec<u8> {
        // magic[0..4] type[4] enabled[5] policy[6] reason[7]
        Wr::ctl(MSG_CLIP_STATE, 8)
            .u8(self.enabled as u8)
            .u8(self.policy)
            .u8(self.reason)
            .done()
    }

    pub fn decode(b: &[u8]) -> Result<ClipState> {
        let mut r = Rd::ctl(b, MSG_CLIP_STATE, 8..=8, "bad ClipState")?;
        Ok(ClipState {
            enabled: r.u8() != 0,
            policy: r.u8(),
            reason: r.u8(),
        })
    }
}

impl ClipOffer {
    pub fn encode(&self) -> Vec<u8> {
        // magic[0..4] type[4] seq[5..9] count[9] then `count` ClipKinds
        let count = self.kinds.len().min(CLIP_MAX_KINDS);
        let mut w = Wr::ctl(MSG_CLIP_OFFER, 10 + self.kinds.len() * 16)
            .u32(self.seq)
            .u8(count as u8);
        for k in &self.kinds[..count] {
            w = put_clip_kind(w, k);
        }
        w.done()
    }

    pub fn decode(b: &[u8]) -> Result<ClipOffer> {
        let mut r = Rd::ctl(b, MSG_CLIP_OFFER, 10.., "bad ClipOffer")?;
        let seq = r.u32();
        let count = r.u8() as usize;
        if count > CLIP_MAX_KINDS {
            return Err(PunktfunkError::InvalidArg("ClipOffer too many kinds"));
        }
        let mut kinds = Vec::with_capacity(count);
        for _ in 0..count {
            kinds.push(get_clip_kind(&mut r)?);
        }
        if r.remaining() != 0 {
            return Err(PunktfunkError::InvalidArg("trailing bytes"));
        }
        Ok(ClipOffer { seq, kinds })
    }
}

impl ClipFetch {
    pub fn encode(&self) -> Vec<u8> {
        // magic[0..4] type[4] seq[5..9] file_index[9..13] mime(len u8 || bytes)[13..]
        let mime = self.mime.as_bytes();
        let n = mime.len().min(CLIP_MAX_MIME);
        Wr::ctl(MSG_CLIP_FETCH, 14 + n)
            .u32(self.seq)
            .u32(self.file_index)
            .u8(n as u8)
            .bytes(&mime[..n])
            .done()
    }

    pub fn decode(b: &[u8]) -> Result<ClipFetch> {
        let mut r = Rd::ctl(b, MSG_CLIP_FETCH, 14.., "bad ClipFetch")?;
        let seq = r.u32();
        let file_index = r.u32();
        let n = r.u8() as usize;
        if n > CLIP_MAX_MIME || r.remaining() != n {
            return Err(PunktfunkError::InvalidArg("bad ClipFetch mime"));
        }
        let mime = String::from_utf8_lossy(r.rest()).into_owned();
        Ok(ClipFetch {
            seq,
            file_index,
            mime,
        })
    }
}

impl ClipFetchHdr {
    pub fn encode(&self) -> Vec<u8> {
        // magic[0..4] type[4] status[5] total_size[6..14]
        Wr::ctl(MSG_CLIP_FETCH_HDR, 14)
            .u8(self.status)
            .u64(self.total_size)
            .done()
    }

    pub fn decode(b: &[u8]) -> Result<ClipFetchHdr> {
        let mut r = Rd::ctl(b, MSG_CLIP_FETCH_HDR, 14..=14, "bad ClipFetchHdr")?;
        Ok(ClipFetchHdr {
            status: r.u8(),
            total_size: r.u64(),
        })
    }
}

// Cursor channel (`design/remote-desktop-sweep.md`). Shape rides the control
// stream; per-frame position rides lossy `0xD0` ([`super::datagram::CursorState`]).
// Active only when [`CLIENT_CAP_CURSOR`](super::caps::CLIENT_CAP_CURSOR) met
// [`HOST_CAP_CURSOR`](super::caps::HOST_CAP_CURSOR) — host then stops compositing.

pub const MSG_CURSOR_SHAPE: u8 = 0x50;
pub const MSG_CURSOR_RENDER: u8 = 0x51;

/// Per-side pixel cap. Control frames are `u16`-length-prefixed (65535).
/// 128×128 RGBA is 65536 B before the 17-byte header; 120² (57.6 KiB +
/// header) fits. Host downscales anything larger.
pub const CURSOR_SHAPE_MAX_SIDE: u16 = 120;

/// `host → client` ([`MSG_CURSOR_SHAPE`]): pointer bitmap changed. Never
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

impl CursorShape {
    pub fn encode(&self) -> Vec<u8> {
        // magic[0..4] type[4] serial[5..9] w[9..11] h[11..13] hot_x[13..15] hot_y[15..17] rgba…
        Wr::ctl(MSG_CURSOR_SHAPE, 17 + self.rgba.len())
            .u32(self.serial)
            .u16(self.w)
            .u16(self.h)
            .u16(self.hot_x)
            .u16(self.hot_y)
            .bytes(&self.rgba)
            .done()
    }

    pub fn decode(b: &[u8]) -> Result<CursorShape> {
        let mut r = Rd::ctl(b, MSG_CURSOR_SHAPE, 17.., "bad CursorShape")?;
        let (serial, w, h, hot_x, hot_y) = (r.u32(), r.u16(), r.u16(), r.u16(), r.u16());
        if w == 0 || h == 0 || w > CURSOR_SHAPE_MAX_SIDE || h > CURSOR_SHAPE_MAX_SIDE {
            return Err(PunktfunkError::InvalidArg("bad CursorShape dims"));
        }
        if r.remaining() != (w as usize) * (h as usize) * 4 {
            return Err(PunktfunkError::InvalidArg("bad CursorShape len"));
        }
        Ok(CursorShape {
            serial,
            w,
            h,
            hot_x,
            hot_y,
            rgba: r.rest().to_vec(),
        })
    }
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

impl CursorRenderMode {
    pub fn encode(&self) -> Vec<u8> {
        // magic[0..4] type[4] client_draws[5]
        Wr::ctl(MSG_CURSOR_RENDER, 6)
            .u8(self.client_draws as u8)
            .done()
    }

    pub fn decode(b: &[u8]) -> Result<CursorRenderMode> {
        let mut r = Rd::ctl(b, MSG_CURSOR_RENDER, 6..=6, "bad CursorRenderMode")?;
        Ok(CursorRenderMode {
            client_draws: r.u8() != 0,
        })
    }
}

// Per-client access (`design/per-client-access.md`). Grant vocabulary lives
// in [`super::access`]; this is the one host → client control message.

/// [`AccessUpdate`]. 0x58: 0x50–0x51 are cursor; 0x40–0x44 are clipboard.
pub const MSG_ACCESS_UPDATE: u8 = 0x58;

/// `host → client` ([`MSG_ACCESS_UPDATE`]): grants or remaining lifetime
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

impl AccessUpdate {
    pub fn encode(&self) -> Vec<u8> {
        // magic[0..4] type[4] grants[5..9] remaining_secs[9..13]
        Wr::ctl(MSG_ACCESS_UPDATE, 13)
            .u32(self.grants)
            .u32(self.remaining_secs)
            .done()
    }

    pub fn decode(b: &[u8]) -> Result<AccessUpdate> {
        let mut r = Rd::ctl(b, MSG_ACCESS_UPDATE, 13..=13, "bad AccessUpdate")?;
        Ok(AccessUpdate {
            grants: r.u32(),
            remaining_secs: r.u32(),
        })
    }
}

/// [`AudioState`]. 0x59: next after [`MSG_ACCESS_UPDATE`].
pub const MSG_AUDIO_STATE: u8 = 0x59;

/// `host → client` ([`MSG_AUDIO_STATE`]): the operator muted this session from the
/// console. The host stops sending audio datagrams; nothing on the client's side is
/// broken, so the client says so instead of concealing a gap. Latest-wins,
/// best-effort — older clients just go quiet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioState {
    pub muted: bool,
}

impl AudioState {
    pub fn encode(&self) -> Vec<u8> {
        // magic[0..4] type[4] muted[5]
        Wr::ctl(MSG_AUDIO_STATE, 6).u8(u8::from(self.muted)).done()
    }

    pub fn decode(b: &[u8]) -> Result<AudioState> {
        let mut r = Rd::ctl(b, MSG_AUDIO_STATE, 6..=6, "bad AudioState")?;
        Ok(AudioState { muted: r.u8() != 0 })
    }
}

/// [`PadSlots`]. 0x5B: 0x5A is the launch outcome.
pub const MSG_PAD_SLOTS: u8 = 0x5B;

/// `host → client` ([`MSG_PAD_SLOTS`]): the OS pad slots this session holds, one
/// bit per slot. Slot `n` is player `n + 1` to a local co-op game, so the client
/// can say which player it is instead of leaving that to whoever moved a stick
/// first. Latest-wins, best-effort; older clients just show nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PadSlots {
    /// Bit `n` set = this session holds OS pad slot `n`. `0` = no pad.
    pub slots: u16,
}

impl PadSlots {
    pub fn encode(&self) -> Vec<u8> {
        // magic[0..4] type[4] slots[5..7]
        Wr::ctl(MSG_PAD_SLOTS, 7).u16(self.slots).done()
    }

    pub fn decode(b: &[u8]) -> Result<PadSlots> {
        let mut r = Rd::ctl(b, MSG_PAD_SLOTS, 7..=7, "bad PadSlots")?;
        Ok(PadSlots { slots: r.u16() })
    }
}

/// [`LaunchOutcome`]. 0x5A: next after [`MSG_AUDIO_STATE`].
pub const MSG_LAUNCH_OUTCOME: u8 = 0x5A;

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

    pub fn encode(&self) -> Vec<u8> {
        // magic[0..4] type[4] kind[5] len[6] message[7..]
        let msg = self.message.as_bytes();
        Wr::ctl(MSG_LAUNCH_OUTCOME, 7 + msg.len())
            .u8(self.kind as u8)
            .u8(msg.len() as u8)
            .bytes(msg)
            .done()
    }

    pub fn decode(b: &[u8]) -> Result<LaunchOutcome> {
        const BAD: &str = "bad LaunchOutcome";
        let bad = || PunktfunkError::InvalidArg(BAD);
        let mut r = Rd::ctl(b, MSG_LAUNCH_OUTCOME, 7.., BAD)?;
        let kind = LaunchOutcomeKind::from_u8(r.u8());
        let len = r.u8() as usize;
        if len > LAUNCH_MESSAGE_MAX || r.remaining() != len {
            return Err(bad());
        }
        Ok(LaunchOutcome {
            kind,
            message: std::str::from_utf8(r.rest())
                .map_err(|_| bad())?
                .to_string(),
        })
    }
}

/// [`InputEdge`]. 0x5C: 0x58–0x5B are access, audio, launch and pad slots.
pub const MSG_INPUT_EDGE: u8 = 0x5C;

/// `client → host` ([`MSG_INPUT_EDGE`]): one input event whose loss would stick — a key
/// press or release — on the control stream instead of the datagram plane, so QUIC resends
/// it and keeps its order. Sent only toward [`HOST_CAP2_INPUT_EDGES`](super::HOST_CAP2_INPUT_EDGES).
/// The payload is the datagram encoding unchanged, so the host feeds both paths into one
/// queue; any kind decodes, and the grants gate it exactly as they gate the datagram.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InputEdge(pub crate::input::InputEvent);

impl InputEdge {
    pub fn encode(&self) -> Vec<u8> {
        // magic[0..4] type[4] event[5..23] (`InputEvent::encode`, tag included)
        Wr::ctl(MSG_INPUT_EDGE, 5 + crate::input::INPUT_WIRE_LEN)
            .bytes(&self.0.encode())
            .done()
    }

    pub fn decode(b: &[u8]) -> Result<InputEdge> {
        const LEN: usize = 5 + crate::input::INPUT_WIRE_LEN;
        let r = Rd::ctl(b, MSG_INPUT_EDGE, LEN..=LEN, "bad InputEdge")?;
        crate::input::InputEvent::decode(r.rest())
            .map(InputEdge)
            .ok_or(PunktfunkError::InvalidArg("bad InputEdge"))
    }
}

#[cfg(test)]
mod tests {
    use crate::config::Mode;
    use crate::quic::*;

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    /// One message's wire bytes after `PKFc` (`504b4663`), both ways: the codecs may
    /// change shape, never the wire.
    macro_rules! pin {
        ($ty:ident, $msg:expr, $hex:literal) => {{
            let m: $ty = $msg;
            let b = m.encode();
            assert_eq!(hex(&b), concat!("504b4663", $hex), stringify!($ty));
            assert_eq!($ty::decode(&b).unwrap(), m, stringify!($ty));
        }};
    }

    #[test]
    fn rate_control_messages_keep_their_wire_bytes() {
        let mode = Mode {
            width: 1920,
            height: 1080,
            refresh_hz: 120,
        };
        pin!(
            Reconfigure,
            Reconfigure { mode },
            "01800700003804000078000000"
        );
        pin!(
            Reconfigured,
            Reconfigured {
                accepted: true,
                mode
            },
            "0201800700003804000078000000"
        );
        pin!(RequestKeyframe, RequestKeyframe, "03");
        pin!(
            LossReport,
            LossReport {
                loss_ppm: 0x0102_0304
            },
            "0404030201"
        );
        pin!(
            SetBitrate,
            SetBitrate {
                bitrate_kbps: 50_000
            },
            "0550c30000"
        );
        pin!(
            BitrateChanged,
            BitrateChanged {
                bitrate_kbps: 50_000,
                reason: Some(AckReason::Cadence)
            },
            "0650c3000002"
        );
        pin!(
            BitrateChanged,
            BitrateChanged {
                bitrate_kbps: 50_000,
                reason: None
            },
            "0650c30000"
        );
        pin!(
            RfiRequest,
            RfiRequest {
                first_frame: 10,
                last_frame: 12
            },
            "070a0000000c000000"
        );
        pin!(
            ShardPayloadChanged,
            ShardPayloadChanged {
                shard_payload: 1200
            },
            "08b004"
        );
        pin!(
            ShardPayloadAck,
            ShardPayloadAck {
                shard_payload: 1200
            },
            "09b004"
        );
        pin!(PipelineGap, PipelineGap { gap_ms: 250 }, "0afa000000");
        pin!(
            DeliveryReport,
            DeliveryReport {
                packets_received: 0x0102_0304_0506_0708
            },
            "0b0807060504030201"
        );
        pin!(
            LinkReport,
            LinkReport {
                proven_kbps: 0x0a0b_0c0d
            },
            "0c0d0c0b0a"
        );
    }

    #[test]
    fn probe_and_clock_messages_keep_their_wire_bytes() {
        pin!(
            ProbeRequest,
            ProbeRequest {
                target_kbps: 100_000,
                duration_ms: 500
            },
            "20a0860100f4010000"
        );
        pin!(
            ProbeResult,
            ProbeResult {
                bytes_sent: 0x1122_3344_5566_7788,
                packets_sent: 1,
                duration_ms: 2,
                wire_packets_sent: 3,
                send_dropped: 4
            },
            "21887766554433221101000000020000000300000004000000"
        );
        pin!(
            ClockProbe,
            ClockProbe {
                t1_ns: 0x0102_0304_0506_0708
            },
            "300807060504030201"
        );
        pin!(
            ClockEcho,
            ClockEcho {
                t1_ns: 1,
                t2_ns: 2,
                t3_ns: 3
            },
            "31010000000000000002000000000000000300000000000000"
        );
        let phase = PhaseReport {
            next_latch_host_ns: 0x0102_0304_0506_0708,
            latch_period_ns: 8_333_333,
            uncertainty_ns: 0x10,
            arrival_lead_ns: 0x20,
            coherence_milli: 900,
        };
        pin!(
            PhaseReport,
            phase,
            "32080706050403020115287f0010000000200000008403"
        );
        pin!(
            PhaseReport,
            PhaseReport {
                coherence_milli: u16::MAX,
                ..phase
            },
            "32080706050403020115287f001000000020000000"
        );
    }

    #[test]
    fn clipboard_messages_keep_their_wire_bytes() {
        pin!(
            ClipControl,
            ClipControl {
                enabled: true,
                flags: CLIP_FLAG_FILES
            },
            "400101"
        );
        pin!(
            ClipState,
            ClipState {
                enabled: true,
                policy: 3,
                reason: 4
            },
            "41010304"
        );
        pin!(
            ClipOffer,
            ClipOffer {
                seq: 7,
                kinds: vec![ClipKind {
                    mime: "text/plain".into(),
                    size_hint: 5
                }]
            },
            "4207000000010a746578742f706c61696e0500000000000000"
        );
        pin!(
            ClipFetch,
            ClipFetch {
                seq: 7,
                file_index: CLIP_FILE_INDEX_NONE,
                mime: "text/plain".into()
            },
            "4307000000ffffffff0a746578742f706c61696e"
        );
        pin!(
            ClipFetchHdr,
            ClipFetchHdr {
                status: CLIP_FETCH_OK,
                total_size: 5
            },
            "44000500000000000000"
        );
    }

    #[test]
    fn cursor_and_session_messages_keep_their_wire_bytes() {
        pin!(
            CursorShape,
            CursorShape {
                serial: 9,
                w: 1,
                h: 1,
                hot_x: 0,
                hot_y: 0,
                rgba: vec![1, 2, 3, 4]
            },
            "5009000000010001000000000001020304"
        );
        pin!(
            CursorRenderMode,
            CursorRenderMode { client_draws: true },
            "5101"
        );
        pin!(
            AccessUpdate,
            AccessUpdate {
                grants: 0x1f,
                remaining_secs: 3600
            },
            "581f000000100e0000"
        );
        pin!(AudioState, AudioState { muted: true }, "5901");
        pin!(
            LaunchOutcome,
            LaunchOutcome::new(LaunchOutcomeKind::Failed, "gone"),
            "5a0404676f6e65"
        );
        pin!(PadSlots, PadSlots { slots: 5 }, "5b0500");
    }

    #[test]
    fn cursor_render_mode_roundtrip() {
        for client_draws in [true, false] {
            let m = CursorRenderMode { client_draws };
            assert_eq!(CursorRenderMode::decode(&m.encode()).unwrap(), m);
        }
        assert!(CursorRenderMode::decode(
            &CursorShape {
                serial: 1,
                w: 1,
                h: 1,
                hot_x: 0,
                hot_y: 0,
                rgba: vec![0; 4]
            }
            .encode()
        )
        .is_err());
    }

    #[test]
    fn phase_report_roundtrip() {
        let pr = PhaseReport {
            next_latch_host_ns: 1_753_900_000_123_456_789,
            latch_period_ns: 8_333_333,
            uncertainty_ns: 900_000,
            arrival_lead_ns: 4_100_000,
            coherence_milli: 742,
        };
        let d = pr.encode();
        assert_eq!(d.len(), 27, "a real coherence rides the v2 tail");
        assert_eq!(PhaseReport::decode(&d).unwrap(), pr);
        // MAX sentinel encodes as the 25-byte v1 form.
        let v1 = PhaseReport {
            coherence_milli: u16::MAX,
            ..pr
        };
        let d1 = v1.encode();
        assert_eq!(d1.len(), 25);
        assert_eq!(&d1[..25], &d[..25], "v1 form is a strict prefix of v2");
        assert_eq!(PhaseReport::decode(&d1).unwrap(), v1);
        assert!(PhaseReport::decode(&ClockProbe { t1_ns: 7 }.encode()).is_err());
        assert!(PhaseReport::decode(&d[..24]).is_err());
        assert!(PhaseReport::decode(&d[..26]).is_err());
    }

    #[test]
    fn reconfigure_roundtrip() {
        let rq = Reconfigure {
            mode: Mode {
                width: 1920,
                height: 1080,
                refresh_hz: 144,
            },
        };
        assert_eq!(Reconfigure::decode(&rq.encode()).unwrap(), rq);
        for accepted in [true, false] {
            let rs = Reconfigured {
                accepted,
                mode: rq.mode,
            };
            assert_eq!(Reconfigured::decode(&rs.encode()).unwrap(), rs);
        }
        assert!(Reconfigure::decode(
            &Reconfigured {
                accepted: true,
                mode: rq.mode
            }
            .encode()
        )
        .is_err());
    }

    #[test]
    fn request_keyframe_roundtrip() {
        let bytes = RequestKeyframe.encode();
        assert!(RequestKeyframe::decode(&bytes).is_ok());
        let mode = Mode {
            width: 1280,
            height: 720,
            refresh_hz: 60,
        };
        assert!(RequestKeyframe::decode(&Reconfigure { mode }.encode()).is_err());
        assert!(Reconfigure::decode(&bytes).is_err());
        assert!(RequestKeyframe::decode(&[bytes.as_slice(), &[0]].concat()).is_err());
    }

    #[test]
    fn link_report_roundtrip() {
        for proven_kbps in [1u32, 778_000, u32::MAX] {
            let r = LinkReport { proven_kbps };
            assert_eq!(LinkReport::decode(&r.encode()).unwrap(), r);
        }
        // Same length as a LossReport: the type byte tells them apart.
        assert!(LinkReport::decode(&LossReport { loss_ppm: 5 }.encode()).is_err());
        assert!(LossReport::decode(&LinkReport { proven_kbps: 5 }.encode()).is_err());
    }

    #[test]
    fn rfi_request_roundtrip() {
        for (first_frame, last_frame) in [(0u32, 0u32), (40, 47), (5, 5), (1_000_000, u32::MAX)] {
            let r = RfiRequest {
                first_frame,
                last_frame,
            };
            assert_eq!(RfiRequest::decode(&r.encode()).unwrap(), r);
        }
        assert!(RfiRequest::decode(&RequestKeyframe.encode()).is_err());
        assert!(RequestKeyframe::decode(
            &RfiRequest {
                first_frame: 1,
                last_frame: 2
            }
            .encode()
        )
        .is_err());
        let bytes = RfiRequest {
            first_frame: 3,
            last_frame: 9,
        }
        .encode();
        assert!(RfiRequest::decode(&[bytes.as_slice(), &[0]].concat()).is_err());
        assert!(RfiRequest::decode(&bytes[..bytes.len() - 1]).is_err());
    }

    #[test]
    fn loss_report_roundtrip() {
        for loss_ppm in [0u32, 1, 12_345, 50_000, 1_000_000] {
            let r = LossReport { loss_ppm };
            assert_eq!(LossReport::decode(&r.encode()).unwrap(), r);
        }
        assert!(LossReport::decode(&RequestKeyframe.encode()).is_err());
        assert!(RequestKeyframe::decode(&LossReport { loss_ppm: 0 }.encode()).is_err());
        assert!(LossReport::decode(
            &[LossReport { loss_ppm: 0 }.encode().as_slice(), &[0]].concat()
        )
        .is_err());
    }

    #[test]
    fn delivery_report_roundtrip() {
        for packets_received in [0u64, 1, 9_999, u32::MAX as u64 + 1, u64::MAX] {
            let r = DeliveryReport { packets_received };
            assert_eq!(DeliveryReport::decode(&r.encode()).unwrap(), r);
        }
        assert!(DeliveryReport::decode(&RequestKeyframe.encode()).is_err());
        assert!(DeliveryReport::decode(&LossReport { loss_ppm: 0 }.encode()).is_err());
    }

    /// Own type byte: [`LossReport`] decode is exact-length, so appending
    /// here would make every shipped host reject adaptive FEC.
    #[test]
    fn the_delivery_count_does_not_disturb_the_loss_report_wire_form() {
        let loss = LossReport { loss_ppm: 42 }.encode();
        assert_eq!(loss.len(), 9, "LossReport must stay the 9-byte wire form");
        assert_eq!(loss[4], MSG_LOSS_REPORT);

        let delivery = DeliveryReport {
            packets_received: 0,
        }
        .encode();
        assert_ne!(
            delivery[4], MSG_LOSS_REPORT,
            "a distinct type byte is what makes an old host ignore it instead of failing"
        );
        assert!(LossReport::decode(&delivery).is_err());
        assert!(DeliveryReport::decode(&loss).is_err());
    }

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

    #[test]
    fn bitrate_messages_roundtrip() {
        let req = SetBitrate {
            bitrate_kbps: 14_000,
        };
        assert_eq!(SetBitrate::decode(&req.encode()).unwrap(), req);
        let ack = BitrateChanged {
            bitrate_kbps: 14_000,
            reason: None,
        };
        assert_eq!(ack.encode().len(), 9, "an old client's ack is unchanged");
        assert_eq!(BitrateChanged::decode(&ack.encode()).unwrap(), ack);
        // Same 9-byte shape as [`LossReport`] — type byte is the only split.
        assert!(LossReport::decode(&req.encode()).is_err());
        assert!(SetBitrate::decode(&ack.encode()).is_err());
        assert!(BitrateChanged::decode(&req.encode()).is_err());
        assert!(SetBitrate::decode(&LossReport { loss_ppm: 7 }.encode()).is_err());
    }

    /// New client ↔ new host: both lengths round-trip, an unknown code reads as
    /// no reason, and nothing else on the control stream decodes as a ten-byte
    /// ack.
    #[test]
    fn a_bitrate_ack_carries_its_reason_or_goes_without_one() {
        for reason in [
            AckReason::Granted,
            AckReason::EncoderLimit,
            AckReason::Cadence,
            AckReason::Governor,
            AckReason::Pinned,
        ] {
            let ack = BitrateChanged {
                bitrate_kbps: 41_852,
                reason: Some(reason),
            };
            let wire = ack.encode();
            assert_eq!(wire.len(), 10);
            assert_eq!(BitrateChanged::decode(&wire).unwrap(), ack);
        }
        // A code from a later host: the rate stands, the reason does not.
        let mut unknown = BitrateChanged {
            bitrate_kbps: 41_852,
            reason: Some(AckReason::Pinned),
        }
        .encode();
        unknown[9] = 9;
        let got = BitrateChanged::decode(&unknown).unwrap();
        assert_eq!(got.bitrate_kbps, 41_852);
        assert_eq!(got.reason, None);
        // Length is still the split: nine, ten, nothing else.
        assert!(BitrateChanged::decode(&unknown[..8]).is_err());
        assert!(BitrateChanged::decode(&[unknown.as_slice(), &[0]].concat()).is_err());
        assert!(SetBitrate::decode(&unknown).is_err());
        assert!(PipelineGap::decode(&unknown).is_err());
        assert!(LossReport::decode(&unknown).is_err());
    }

    #[test]
    fn pipeline_gap_roundtrips() {
        for gap_ms in [1u32, 401, 60_000, u32::MAX] {
            let m = PipelineGap { gap_ms };
            assert_eq!(PipelineGap::decode(&m.encode()).unwrap(), m);
        }
        // Same 9-byte shape as the rate-control messages. A gap decoded as
        // [`SetBitrate`] would retarget the encoder to 401 kbps.
        let gap = PipelineGap { gap_ms: 401 }.encode();
        assert_eq!(gap[4], MSG_PIPELINE_GAP);
        assert!(LossReport::decode(&gap).is_err());
        assert!(SetBitrate::decode(&gap).is_err());
        assert!(BitrateChanged::decode(&gap).is_err());
        assert!(PipelineGap::decode(&LossReport { loss_ppm: 401 }.encode()).is_err());
        assert!(PipelineGap::decode(&SetBitrate { bitrate_kbps: 401 }.encode()).is_err());
        assert!(PipelineGap::decode(
            &BitrateChanged {
                bitrate_kbps: 401,
                reason: None,
            }
            .encode()
        )
        .is_err());
        assert!(ShardPayloadAck::decode(&gap).is_err());
        assert!(PipelineGap::decode(&[gap.as_slice(), &[0]].concat()).is_err());
        assert!(PipelineGap::decode(&gap[..gap.len() - 1]).is_err());
    }

    #[test]
    fn shard_payload_messages_roundtrip() {
        for shard_payload in [512u16, 1216, 1408, 8908] {
            let chg = ShardPayloadChanged { shard_payload };
            assert_eq!(ShardPayloadChanged::decode(&chg.encode()).unwrap(), chg);
            let ack = ShardPayloadAck { shard_payload };
            assert_eq!(ShardPayloadAck::decode(&ack.encode()).unwrap(), ack);
            // Identical payload — an ack must never re-decode as a change.
            assert!(ShardPayloadChanged::decode(&ack.encode()).is_err());
            assert!(ShardPayloadAck::decode(&chg.encode()).is_err());
        }
        let bytes = ShardPayloadChanged { shard_payload: 512 }.encode();
        assert!(ShardPayloadChanged::decode(&[bytes.as_slice(), &[0]].concat()).is_err());
        assert!(ShardPayloadChanged::decode(&bytes[..bytes.len() - 1]).is_err());
        assert!(ShardPayloadChanged::decode(
            &RfiRequest {
                first_frame: 1,
                last_frame: 2
            }
            .encode()
        )
        .is_err());
    }

    /// The delivery types and the shaped probe: exact-length, like every type here. An
    /// older peer rejects each as an unknown type, which is why a client sends them only
    /// after the host answered the `Start` tag.
    #[test]
    fn delivery_messages_keep_their_wire_bytes() {
        pin!(SetDelivery, SetDelivery { profile: 2 }, "0d02");
        pin!(
            DeliveryChanged,
            DeliveryChanged {
                profile: 1,
                forced: true
            },
            "0e0101"
        );
        pin!(
            HostFacts,
            HostFacts {
                iface_kind: IFACE_KIND_ETHERNET,
                link_mbps: 2500,
                sndbuf_kb: 32768,
                forced_profile: 0
            },
            "0f01c40900000080000000"
        );
        pin!(
            ProbeShaped,
            ProbeShaped {
                target_kbps: 100_000,
                duration_ms: 500,
                burst_hz: 60,
                group_bytes: 65_536,
                group_rate_kbps: 800_000
            },
            "22a0860100f40100003c000000010000350c00"
        );
        let plain = ProbeRequest {
            target_kbps: 100_000,
            duration_ms: 500,
        };
        let shaped: ProbeShaped = plain.into();
        assert_eq!(
            (shaped.burst_hz, shaped.group_bytes, shaped.group_rate_kbps),
            (0, 0, 0)
        );
        assert!(ProbeShaped::decode(&plain.encode()).is_err());
        assert!(ProbeRequest::decode(&shaped.encode()).is_err());
        assert!(SetDelivery::decode(
            &DeliveryChanged {
                profile: 1,
                forced: false
            }
            .encode()
        )
        .is_err());
    }

    #[test]
    fn probe_messages_roundtrip() {
        let req = ProbeRequest {
            target_kbps: 250_000,
            duration_ms: 2000,
        };
        assert_eq!(ProbeRequest::decode(&req.encode()).unwrap(), req);
        let res = ProbeResult {
            bytes_sent: 62_500_000,
            packets_sent: 480,
            duration_ms: 2003,
            wire_packets_sent: 41_000,
            send_dropped: 1_200,
        };
        assert_eq!(ProbeResult::decode(&res.encode()).unwrap(), res);
        assert_eq!(res.encode().len(), 29);
        // 21-byte pre-wire-stats form: new fields decode as 0.
        let legacy = {
            let full = res.encode();
            full[..21].to_vec()
        };
        let decoded = ProbeResult::decode(&legacy).unwrap();
        assert_eq!(decoded.wire_packets_sent, 0);
        assert_eq!(decoded.send_dropped, 0);
        assert_eq!(decoded.bytes_sent, res.bytes_sent);
        assert!(ProbeRequest::decode(&res.encode()).is_err());
        assert!(Reconfigure::decode(&req.encode()).is_err());
        assert!(ProbeResult::decode(&req.encode()).is_err());
    }

    #[test]
    fn clock_messages_roundtrip() {
        let probe = ClockProbe {
            t1_ns: 1_700_000_000_123,
        };
        assert_eq!(ClockProbe::decode(&probe.encode()).unwrap(), probe);
        let echo = ClockEcho {
            t1_ns: 1_700_000_000_123,
            t2_ns: 1_700_000_050_456,
            t3_ns: 1_700_000_050_789,
        };
        assert_eq!(ClockEcho::decode(&echo.encode()).unwrap(), echo);
        assert!(ClockProbe::decode(&echo.encode()).is_err());
        assert!(ProbeRequest::decode(&probe.encode()).is_err());
        assert!(ClockEcho::decode(&probe.encode()).is_err());
    }

    #[test]
    fn clip_control_roundtrip() {
        for (enabled, flags) in [
            (true, 0u8),
            (false, 0),
            (true, CLIP_FLAG_FILES),
            (false, 0xFF),
        ] {
            let m = ClipControl { enabled, flags };
            assert_eq!(ClipControl::decode(&m.encode()).unwrap(), m);
        }
        assert!(ClipControl::decode(
            &ClipState {
                enabled: true,
                policy: 0,
                reason: 0
            }
            .encode()
        )
        .is_err());
        let bytes = ClipControl {
            enabled: true,
            flags: 0,
        }
        .encode();
        assert!(ClipControl::decode(&[bytes.as_slice(), &[0]].concat()).is_err());
        assert!(ClipControl::decode(&bytes[..bytes.len() - 1]).is_err());
    }

    #[test]
    fn clip_state_roundtrip() {
        let cases = [
            ClipState {
                enabled: true,
                policy: CLIP_POLICY_TEXT | CLIP_POLICY_FILES,
                reason: CLIP_REASON_OK,
            },
            ClipState {
                enabled: false,
                policy: 0,
                reason: CLIP_REASON_BACKEND_UNAVAILABLE,
            },
            ClipState {
                enabled: true,
                policy: CLIP_POLICY_TEXT,
                reason: CLIP_REASON_NO_FILES,
            },
            ClipState {
                enabled: false,
                policy: CLIP_POLICY_TEXT | CLIP_POLICY_FILES,
                reason: CLIP_REASON_NOT_PERMITTED,
            },
        ];
        for m in cases {
            assert_eq!(ClipState::decode(&m.encode()).unwrap(), m);
        }
        // A reused value would mislabel refusals on a shipped client, not fail.
        let reasons = [
            CLIP_REASON_OK,
            CLIP_REASON_BACKEND_UNAVAILABLE,
            CLIP_REASON_TAKEN_OVER,
            CLIP_REASON_POLICY_DISABLED,
            CLIP_REASON_NO_FILES,
            CLIP_REASON_NOT_PERMITTED,
        ];
        for (i, a) in reasons.iter().enumerate() {
            for b in &reasons[i + 1..] {
                assert_ne!(a, b, "CLIP_REASON_* values must be distinct");
            }
        }
        assert!(ClipState::decode(
            &ClipControl {
                enabled: true,
                flags: 0
            }
            .encode()
        )
        .is_err());
        let bytes = cases[0].encode();
        assert!(ClipState::decode(&bytes[..bytes.len() - 1]).is_err());
    }

    #[test]
    fn clip_offer_roundtrip() {
        let cases = [
            ClipOffer {
                seq: 0,
                kinds: vec![],
            },
            ClipOffer {
                seq: 1,
                kinds: vec![ClipKind {
                    mime: "text/plain;charset=utf-8".into(),
                    size_hint: 12,
                }],
            },
            ClipOffer {
                seq: u32::MAX,
                kinds: vec![
                    ClipKind {
                        mime: "text/plain;charset=utf-8".into(),
                        size_hint: 0,
                    },
                    ClipKind {
                        mime: "text/html".into(),
                        size_hint: 4096,
                    },
                    ClipKind {
                        mime: "image/png".into(),
                        size_hint: 1 << 30,
                    },
                    ClipKind {
                        mime: "application/x-punktfunk-files".into(),
                        size_hint: 5_000_000_000,
                    },
                ],
            },
        ];
        for m in &cases {
            assert_eq!(&ClipOffer::decode(&m.encode()).unwrap(), m);
        }
        let mut padded = cases[1].encode();
        padded.push(0);
        assert!(ClipOffer::decode(&padded).is_err());
        let mut over = cases[0].encode();
        over[9] = (CLIP_MAX_KINDS + 1) as u8;
        assert!(ClipOffer::decode(&over).is_err());
        assert!(ClipOffer::decode(
            &ClipControl {
                enabled: true,
                flags: 0
            }
            .encode()
        )
        .is_err());
    }

    #[test]
    fn clip_fetch_roundtrip() {
        let cases = [
            ClipFetch {
                seq: 1,
                file_index: CLIP_FILE_INDEX_NONE,
                mime: "text/plain;charset=utf-8".into(),
            },
            ClipFetch {
                seq: 7,
                file_index: 0,
                mime: "application/x-punktfunk-files".into(),
            },
            ClipFetch {
                seq: u32::MAX,
                file_index: 41,
                mime: String::new(),
            },
        ];
        for m in &cases {
            assert_eq!(&ClipFetch::decode(&m.encode()).unwrap(), m);
        }
        let bytes = cases[0].encode();
        assert!(ClipFetch::decode(&[bytes.as_slice(), &[0]].concat()).is_err());
        assert!(ClipFetch::decode(&bytes[..bytes.len() - 1]).is_err());
        // Fetch-stream vs control-stream: neither decoder accepts the other.
        assert!(ClipOffer::decode(&cases[0].encode()).is_err());
        assert!(ClipFetch::decode(
            &ClipOffer {
                seq: 1,
                kinds: vec![]
            }
            .encode()
        )
        .is_err());
    }

    #[test]
    fn clip_fetch_hdr_roundtrip() {
        for (status, total_size) in [
            (CLIP_FETCH_OK, 15u64),
            (CLIP_FETCH_STALE, 0),
            (CLIP_FETCH_UNAVAILABLE, 0),
            (CLIP_FETCH_DENIED, 0),
            (CLIP_FETCH_OK, u64::MAX),
        ] {
            let m = ClipFetchHdr { status, total_size };
            assert_eq!(ClipFetchHdr::decode(&m.encode()).unwrap(), m);
        }
        let bytes = ClipFetchHdr {
            status: CLIP_FETCH_OK,
            total_size: 1,
        }
        .encode();
        assert!(ClipFetchHdr::decode(&[bytes.as_slice(), &[0]].concat()).is_err());
        assert!(ClipFetchHdr::decode(&bytes[..bytes.len() - 1]).is_err());
    }
    #[test]
    fn cursor_shape_roundtrip() {
        let s = CursorShape {
            serial: 7,
            w: 2,
            h: 3,
            hot_x: 1,
            hot_y: 2,
            rgba: (0..2 * 3 * 4).map(|i| i as u8).collect(),
        };
        assert_eq!(CursorShape::decode(&s.encode()).unwrap(), s);
        let side = CURSOR_SHAPE_MAX_SIDE;
        let big = CursorShape {
            serial: u32::MAX,
            w: side,
            h: side,
            hot_x: side - 1,
            hot_y: 0,
            rgba: vec![0xAB; side as usize * side as usize * 4],
        };
        let bytes = big.encode();
        assert!(bytes.len() <= u16::MAX as usize, "must fit a control frame");
        assert_eq!(CursorShape::decode(&bytes).unwrap(), big);
        let mut zero = s.encode();
        zero[9] = 0;
        zero[10] = 0;
        assert!(CursorShape::decode(&zero).is_err());
        let mut oversize = s.encode();
        oversize[9..11].copy_from_slice(&(CURSOR_SHAPE_MAX_SIDE + 1).to_le_bytes());
        assert!(CursorShape::decode(&oversize).is_err());
        let mut short = s.encode();
        short.pop();
        assert!(CursorShape::decode(&short).is_err());
        assert!(ClipState::decode(&s.encode()).is_err());
    }

    #[test]
    fn access_update_roundtrip() {
        for (grants, remaining_secs) in [
            (GRANT_ALL, 0u32),
            (GRANT_PRESET_CONTROLLER_ONLY, 300),
            (GRANT_PRESET_VIEW_ONLY, 60),
            (GRANT_GAMEPAD | GRANT_CLIPBOARD, u32::MAX),
        ] {
            let m = AccessUpdate {
                grants,
                remaining_secs,
            };
            assert_eq!(AccessUpdate::decode(&m.encode()).unwrap(), m);
        }
        let bytes = AccessUpdate {
            grants: GRANT_ALL,
            remaining_secs: 1,
        }
        .encode();
        assert_eq!(bytes[4], MSG_ACCESS_UPDATE);
        assert!(ClipState::decode(&bytes).is_err());
        assert!(CursorRenderMode::decode(&bytes).is_err());
        assert!(AccessUpdate::decode(&[bytes.as_slice(), &[0]].concat()).is_err());
        assert!(AccessUpdate::decode(&bytes[..bytes.len() - 1]).is_err());
    }

    /// Same six-byte shape as `CursorRenderMode`, so the type byte is the only thing
    /// that tells them apart — decode must reject the neighbour, not read its flag.
    #[test]
    fn audio_state_roundtrip() {
        for muted in [true, false] {
            let m = AudioState { muted };
            assert_eq!(AudioState::decode(&m.encode()).unwrap(), m);
        }
        let bytes = AudioState { muted: true }.encode();
        assert_eq!(bytes[4], MSG_AUDIO_STATE);
        assert!(CursorRenderMode::decode(&bytes).is_err());
        assert!(AudioState::decode(&CursorRenderMode { client_draws: true }.encode()).is_err());
        assert!(AudioState::decode(&bytes[..bytes.len() - 1]).is_err());
    }

    #[test]
    fn pad_slots_roundtrip() {
        for slots in [0u16, 0b1, 0b1010, u16::MAX] {
            let m = PadSlots { slots };
            assert_eq!(PadSlots::decode(&m.encode()).unwrap(), m);
        }
        let bytes = PadSlots { slots: 0b10 }.encode();
        assert_eq!(bytes[4], MSG_PAD_SLOTS);
        assert!(PadSlots::decode(&AudioState { muted: true }.encode()).is_err());
        assert!(PadSlots::decode(&bytes[..bytes.len() - 1]).is_err());
    }

    /// Every variant back off the wire, and the length byte honoured: the message is
    /// the only variable-length payload in this family, so a lie about it must not slice.
    #[test]
    fn launch_outcome_roundtrip() {
        for kind in [
            LaunchOutcomeKind::Spawned,
            LaunchOutcomeKind::Adopted,
            LaunchOutcomeKind::AdoptedUnknown,
            LaunchOutcomeKind::Refused,
            LaunchOutcomeKind::Failed,
            LaunchOutcomeKind::SignInNeeded,
        ] {
            for text in [
                "",
                "Couldn't start Quail \u{2014} it isn't installed on the host.",
            ] {
                let m = LaunchOutcome::new(kind, text);
                assert_eq!(LaunchOutcome::decode(&m.encode()).unwrap(), m);
                assert_eq!(m.encode()[4], MSG_LAUNCH_OUTCOME);
                assert_eq!(m.encode()[5], kind as u8);
            }
        }
        // Unknown kind reads as Spawned: an older client waits rather than alarms.
        let mut bytes = LaunchOutcome::new(LaunchOutcomeKind::Failed, "x").encode();
        bytes[5] = 200;
        assert_eq!(
            LaunchOutcome::decode(&bytes).unwrap().kind,
            LaunchOutcomeKind::Spawned
        );
        assert!(!LaunchOutcomeKind::Spawned.needs_telling());
        assert!(LaunchOutcomeKind::Failed.needs_telling());
        // A sign-in is the one telling kind that is not a failure: the sentence is the point.
        assert!(LaunchOutcomeKind::SignInNeeded.needs_telling());
        assert_eq!(
            LaunchOutcome::new(LaunchOutcomeKind::SignInNeeded, "Sign in.").notice(),
            Some("Sign in.")
        );
        assert_eq!(
            LaunchOutcome::new(LaunchOutcomeKind::Failed, "Quail closed.").notice(),
            Some("Quail closed.")
        );
        assert_eq!(
            LaunchOutcome::new(LaunchOutcomeKind::Adopted, "Picked it up.").notice(),
            None
        );
        assert_eq!(
            LaunchOutcome::new(LaunchOutcomeKind::Refused, "").notice(),
            None
        );

        let good = LaunchOutcome::new(LaunchOutcomeKind::Failed, "x").encode();
        assert!(LaunchOutcome::decode(&good[..good.len() - 1]).is_err());
        assert!(LaunchOutcome::decode(&[good.as_slice(), &[0]].concat()).is_err());
        assert!(LaunchOutcome::decode(&AudioState { muted: true }.encode()).is_err());
        assert!(AudioState::decode(&good).is_err());
    }

    /// The message is capped and stripped where it is built, not where it is shown:
    /// a control character from a game's own output must never reach a terminal.
    #[test]
    fn launch_outcome_message_is_bounded_and_plain() {
        let long = LaunchOutcome::new(LaunchOutcomeKind::Failed, &"\u{e9}".repeat(400));
        assert!(long.message.len() <= LAUNCH_MESSAGE_MAX);
        assert_eq!(LaunchOutcome::decode(&long.encode()).unwrap(), long);
        assert_eq!(
            LaunchOutcome::new(LaunchOutcomeKind::Refused, "  a\u{7}b\n  ").message,
            "ab"
        );
    }

    /// The payload is the datagram bytes, tag included, so one decoder serves both paths.
    #[test]
    fn input_edge_carries_the_datagram_bytes() {
        use crate::input::{InputEvent, InputKind};
        let key = |kind, code| InputEvent {
            kind,
            _pad: [0; 3],
            code,
            x: 0,
            y: 0,
            flags: 0,
        };
        pin!(
            InputEdge,
            InputEdge(key(InputKind::KeyDown, 0x41)),
            "5cc80041000000000000000000000000000000"
        );
        pin!(
            InputEdge,
            InputEdge(key(InputKind::KeyUp, 0xA0)),
            "5cc801a0000000000000000000000000000000"
        );
        let good = InputEdge(key(InputKind::KeyDown, 0x41)).encode();
        assert_eq!(&good[5..], &key(InputKind::KeyDown, 0x41).encode());
        assert!(InputEdge::decode(&good[..good.len() - 1]).is_err());
        assert!(InputEdge::decode(&[good.as_slice(), &[0]].concat()).is_err());
        assert!(PadSlots::decode(&good).is_err());
        let mut unknown_kind = good.clone();
        unknown_kind[6] = 0xFF;
        assert!(InputEdge::decode(&unknown_kind).is_err());
    }
}
