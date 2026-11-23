//! Every `PUNKTFUNK_*` knob the native plane reads, and the only place it reads one.
//!
//! [`HostKnobs`] is the process's, read once. [`SessionKnobs`] is read at each admission and
//! handed down, so a console change to a registry row applies from the next session. Only a
//! value a restart alone can change lives in [`HostKnobs`]: a setting never needs a restart.

use super::Punktfunk1Source;
use std::time::Duration;

/// Whether the native plane listens on IPv6, and whom it answers there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ipv6 {
    Off,
    /// A peer the public internet cannot have carried: both ends outside 2000::/3.
    Private,
    /// Every peer, as the IPv4 listener does.
    Public,
}

impl Ipv6 {
    /// From `(the ipv6 row, PUNKTFUNK_IPV6_PUBLIC)`. A seat host takes the pair its box handed
    /// on, so a client the door redirects over IPv6 finds its seat listening. Any other host,
    /// and a seat whose box wrote none, takes its own.
    fn resolve(of_box: Option<(bool, bool)>, own: (bool, bool)) -> Ipv6 {
        match of_box.unwrap_or(own) {
            (false, _) => Ipv6::Off,
            (true, false) => Ipv6::Private,
            (true, true) => Ipv6::Public,
        }
    }
}

/// The knobs the process reads once.
pub(crate) struct HostKnobs {
    /// Row `ipv6` and `PUNKTFUNK_IPV6_PUBLIC` ([`Ipv6::resolve`]).
    pub(crate) ipv6: Ipv6,
    /// `PUNKTFUNK_SOURCE=software`: a software-encoded test picture instead of a display, so a
    /// headless box with the management API can prove a client end to end. Anything else is the
    /// display.
    pub(crate) source: Punktfunk1Source,
    /// `PUNKTFUNK_IDLE_TIMEOUT_MS`; `None` (unset/invalid/zero) = `endpoint::DEFAULT_IDLE_TIMEOUT`.
    /// Clamped downstream to ≥1 s with a keep-alive that scales, so a live session never
    /// false-closes.
    pub(crate) idle_timeout: Option<Duration>,
    /// `PUNKTFUNK_ACKED_REFS`: `on` holds confirmed references all session, `off` never takes
    /// them, `None` follows the link.
    pub(crate) acked_refs: Option<bool>,
    /// `PUNKTFUNK_IDD_ADAPTIVE`, on by default. Off skips the cadence adaptation: no cadence
    /// verdict for the ABR climb gate, and the encoder never escalates to pipelined retrieve.
    pub(crate) idd_adaptive: bool,
    /// `PUNKTFUNK_XBOX_BACKEND` as set (`hid` / `xusb`), for `gamepad::windows_xbox_hid`.
    #[cfg(target_os = "windows")]
    pub(crate) xbox_backend: Option<String>,
}

impl HostKnobs {
    /// The process's knobs, read on first use.
    pub(crate) fn get() -> &'static HostKnobs {
        static KNOBS: std::sync::OnceLock<HostKnobs> = std::sync::OnceLock::new();
        KNOBS.get_or_init(|| HostKnobs {
            ipv6: {
                let own = pf_host_config::config();
                let of_box = pf_host_config::seat_defaults::of_box();
                Ipv6::resolve(
                    of_box.map(|d| (d.ipv6, d.ipv6_public)),
                    (own.ipv6, own.ipv6_public),
                )
            },
            source: match std::env::var("PUNKTFUNK_SOURCE").as_deref() {
                Ok("software") => Punktfunk1Source::Software,
                _ => Punktfunk1Source::Virtual,
            },
            idle_timeout: pf_host_config::knob("PUNKTFUNK_IDLE_TIMEOUT_MS")
                .and_then(|s| s.trim().parse::<u64>().ok())
                .filter(|&ms| ms > 0)
                .map(Duration::from_millis),
            acked_refs: match std::env::var("PUNKTFUNK_ACKED_REFS").as_deref() {
                Ok("on") => Some(true),
                Ok("off") => Some(false),
                _ => None,
            },
            idd_adaptive: pf_host_config::env_on("PUNKTFUNK_IDD_ADAPTIVE").unwrap_or(true),
            #[cfg(target_os = "windows")]
            xbox_backend: std::env::var("PUNKTFUNK_XBOX_BACKEND").ok(),
        })
    }
}

/// The knobs each admission reads: the console's registry rows, and the env knobs a session
/// reads per session.
#[derive(Clone, Copy, Debug)]
pub(crate) struct SessionKnobs {
    /// `PUNKTFUNK_TEST_FEEDBACK=1` on the synthetic source: a scripted rumble and HID output
    /// burst for a loopback client, with no real pad.
    pub(crate) test_feedback: bool,
    /// `PUNKTFUNK_PYROWAVE_MAX_MBPS`, in kbps: every PyroWave pin's cap. `None` when unset,
    /// zero or invalid (no cap).
    pub(crate) pyrowave_max_kbps: Option<u32>,
    /// `PUNKTFUNK_FEC_PCT`, clamped to ≤ 90, pins recovery and disables adaptive FEC. `None` ⇒
    /// adaptive. `0` disables FEC.
    pub(crate) fec_pct: Option<u8>,
    /// `PUNKTFUNK_10BIT_SDR_WIDEN`: an 8-bit desktop may widen to 10-bit SDR.
    pub(crate) sdr10_widen: bool,
    /// `PUNKTFUNK_STEAM_FORCE=1` keeps a virtual Deck beside the host's own Deck controller.
    pub(crate) steam_force: bool,
    /// `PUNKTFUNK_WIRE_MTU`; a value that is not a number is logged and ignored.
    pub(crate) wire_mtu: Option<usize>,
    /// `PUNKTFUNK_RUMBLE_TTL_MS` as given; the input plane clamps it.
    pub(crate) rumble_ttl_ms: Option<u16>,
    /// `PUNKTFUNK_STREAMED_AU`, on by default; off reverts to whole-AU sends.
    pub(crate) streamed_au: bool,
    /// `PUNKTFUNK_SLICE_STREAM`, on by default.
    pub(crate) slice_stream: bool,
    /// `PUNKTFUNK_PAD_AUDIO`, the controller speaker row.
    pub(crate) pad_audio: bool,
    /// `PUNKTFUNK_SESSION_WATCH`; `None` is auto.
    pub(crate) session_watch: Option<bool>,
}

impl SessionKnobs {
    pub(crate) fn read() -> SessionKnobs {
        SessionKnobs {
            test_feedback: std::env::var("PUNKTFUNK_TEST_FEEDBACK").as_deref() == Ok("1"),
            pyrowave_max_kbps: pf_host_config::knob("PUNKTFUNK_PYROWAVE_MAX_MBPS")
                .and_then(|s| s.trim().parse::<u32>().ok())
                .filter(|&m| m > 0)
                .map(|m| m.saturating_mul(1000)),
            fec_pct: std::env::var("PUNKTFUNK_FEC_PCT")
                .ok()
                .and_then(|s| s.trim().parse::<u8>().ok())
                .map(|p| p.min(90)),
            sdr10_widen: pf_host_config::row_bool("PUNKTFUNK_10BIT_SDR_WIDEN"),
            steam_force: pf_host_config::env_on("PUNKTFUNK_STEAM_FORCE") == Some(true),
            wire_mtu: std::env::var("PUNKTFUNK_WIRE_MTU").ok().and_then(|v| {
                let mtu = v.trim().parse::<usize>().ok();
                if mtu.is_none() {
                    tracing::warn!(value = %v, "PUNKTFUNK_WIRE_MTU is not a number — ignoring it");
                }
                mtu
            }),
            rumble_ttl_ms: std::env::var("PUNKTFUNK_RUMBLE_TTL_MS")
                .ok()
                .and_then(|s| s.parse::<u16>().ok()),
            streamed_au: pf_host_config::env_on("PUNKTFUNK_STREAMED_AU").unwrap_or(true),
            slice_stream: pf_host_config::env_on("PUNKTFUNK_SLICE_STREAM").unwrap_or(true),
            pad_audio: pf_host_config::row_bool("PUNKTFUNK_PAD_AUDIO"),
            session_watch: pf_host_config::row_tri("PUNKTFUNK_SESSION_WATCH"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Ipv6;

    /// The row turns the listener on, the env knob opens it, and a seat host follows its box
    /// whatever its own settings say.
    #[test]
    fn ipv6_follows_the_row_and_a_seat_follows_its_box() {
        assert_eq!(Ipv6::resolve(None, (false, true)), Ipv6::Off);
        assert_eq!(Ipv6::resolve(None, (true, false)), Ipv6::Private);
        assert_eq!(Ipv6::resolve(None, (true, true)), Ipv6::Public);
        assert_eq!(
            Ipv6::resolve(Some((true, false)), (false, false)),
            Ipv6::Private
        );
        assert_eq!(Ipv6::resolve(Some((false, false)), (true, true)), Ipv6::Off);
    }
}
