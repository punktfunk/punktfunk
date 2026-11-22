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
    /// The `ipv6` row, then `PUNKTFUNK_IPV6_PUBLIC` as set. Only `1`, `true`, `on` or `yes`
    /// opens the listener to public addresses: a blank or misspelt value never does. A box
    /// with seats stays on IPv4: its seat hosts do not take this row, so a client the door
    /// redirects over IPv6 would find nobody.
    fn resolve(row: bool, public: Option<&str>, seats: bool) -> Ipv6 {
        let public = public.map(|v| v.trim().to_ascii_lowercase());
        match (row && !seats, public.as_deref()) {
            (false, _) => Ipv6::Off,
            (true, Some("1" | "true" | "on" | "yes")) => Ipv6::Public,
            (true, _) => Ipv6::Private,
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
                let role = pf_paths::seat::role();
                let seats = role.is_door() || role.is_seat_host();
                let row = pf_host_config::row_bool("PUNKTFUNK_IPV6");
                if row && seats {
                    tracing::warn!("IPv6 is on but a box with seats serves IPv4 only");
                }
                let public = std::env::var("PUNKTFUNK_IPV6_PUBLIC").ok();
                Ipv6::resolve(row, public.as_deref(), seats)
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

    /// The row turns the listener on; only an explicit yes opens it to public addresses.
    #[test]
    fn ipv6_answers_public_addresses_only_on_an_explicit_yes() {
        assert_eq!(Ipv6::resolve(false, Some("1"), false), Ipv6::Off);
        assert_eq!(Ipv6::resolve(true, None, false), Ipv6::Private);
        for yes in ["1", "true", "on", "yes", " ON "] {
            assert_eq!(Ipv6::resolve(true, Some(yes), false), Ipv6::Public, "{yes}");
        }
        for not_yes in ["", "0", "off", "no", "false", "2", "public"] {
            let got = Ipv6::resolve(true, Some(not_yes), false);
            assert_eq!(got, Ipv6::Private, "{not_yes:?}");
        }
        // A door and its seat hosts stay on IPv4.
        assert_eq!(Ipv6::resolve(true, Some("1"), true), Ipv6::Off);
    }
}
