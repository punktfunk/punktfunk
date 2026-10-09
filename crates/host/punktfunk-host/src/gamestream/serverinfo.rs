//! The `/serverinfo` capability/status XML Moonlight GETs before pairing and each launch.

use super::{APP_VERSION, GFE_VERSION, SCM_HEVC, SERVER_CODEC_MODE_SUPPORT};
use crate::host::Host;

/// GFE's advertised HEVC luma-pixel ceiling. Moonlight rejects a mode above this.
const MAX_LUMA_PIXELS_HEVC: u64 = 1_869_449_984;

/// Build the `<root status_code="200">…</root>` document. `https` selects the
/// paired-HTTPS variant (real MAC); `paired` is whether the HTTPS peer's client
/// cert is on the allow-list (`PairStatus`). Element names are case-sensitive
/// and match moonlight-common-c.
///
/// `current_game` is the running app id **as this caller may see it** (0 = none).
/// Moonlight keys Resume/Quit on `currentgame != 0` and `_SERVER_BUSY` together;
/// a non-owner shown the live id would hit owner-only `/resume`/`/cancel`.
///
/// `reached` is the host address the request arrived on: `LocalIP` and the MAC come from that
/// interface, else from the default route's.
pub fn serverinfo_xml(
    host: &Host,
    https: bool,
    paired: bool,
    current_game: u32,
    reached: Option<std::net::IpAddr>,
) -> String {
    let reached = reached
        .map(|ip| ip.to_canonical())
        .filter(|ip| !ip.is_loopback() && !ip.is_unspecified());
    // Plain HTTP has no per-client identity, so the MAC is zeros. HTTPS is the reached NIC's
    // MAC: Moonlight persists it for Wake-on-LAN.
    let real_mac = if https { host_mac(reached) } else { None };
    let mac = real_mac.as_deref().unwrap_or("00:00:00:00:00:00");
    let pair_status = u8::from(paired);
    let state = if current_game != 0 {
        "SUNSHINE_SERVER_BUSY"
    } else {
        "SUNSHINE_SERVER_FREE"
    };
    let codec_mode_support = codec_mode_support();
    // Follow the mask: `0` is "no HEVC" in this field's terms. The ceiling beside a
    // mask without HEVC would contradict the same document.
    let max_luma_hevc = if codec_mode_support & SCM_HEVC != 0 {
        MAX_LUMA_PIXELS_HEVC
    } else {
        0
    };
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<root status_code="200">
<hostname>{hostname}</hostname>
<appversion>{APP_VERSION}</appversion>
<GfeVersion>{GFE_VERSION}</GfeVersion>
<uniqueid>{uniqueid}</uniqueid>
<HttpsPort>{https_port}</HttpsPort>
<ExternalPort>{http_port}</ExternalPort>
<MaxLumaPixelsHEVC>{max_luma_hevc}</MaxLumaPixelsHEVC>
<mac>{mac}</mac>
<LocalIP>{local_ip}</LocalIP>
<ServerCodecModeSupport>{codec_mode_support}</ServerCodecModeSupport>
<PairStatus>{pair_status}</PairStatus>
<currentgame>{current_game}</currentgame>
<state>{state}</state>
</root>
"#,
        // Free text from the console: a raw `&` or `<` makes Moonlight's parser drop the host.
        hostname = super::apps::xml_escape(&host.hostname),
        uniqueid = host.uniqueid,
        https_port = host.https_port,
        http_port = host.http_port,
        local_ip = reached.unwrap_or_else(|| host.local_ip()),
    )
}

/// Wake MAC of the NIC holding `reached`, else of the routed NIC (`crate::wol::wake_macs`).
/// The routed one is cached on first success only: a latched miss would advertise zeros after
/// a boot with no address yet.
fn host_mac(reached: Option<std::net::IpAddr>) -> Option<String> {
    if let Some(mac) = reached.and_then(|ip| crate::wol::wake_macs(ip).into_iter().next()) {
        return Some(mac);
    }
    static MAC: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
    let mut cached = MAC.lock().unwrap_or_else(|p| p.into_inner());
    if cached.is_none() {
        *cached = crate::host::primary_local_ip()
            .map(crate::wol::wake_macs)
            .unwrap_or_default()
            .into_iter()
            .next();
    }
    cached.clone()
}

/// `<ServerCodecModeSupport>`: SDR baseline ([`base_codec_mode_support`]) plus each
/// codec's 10-bit bit the host can actually deliver ([`apply_hdr`] /
/// [`crate::gamestream::host_hdr_capable`]). Moonlight offers its HDR toggle only
/// when a 10-bit bit is set.
pub(super) fn codec_mode_support() -> u32 {
    use crate::encode::Codec;
    let hdr = crate::gamestream::host_hdr_capable();
    apply_hdr(
        base_codec_mode_support(),
        hdr && crate::encode::can_encode_10bit(Codec::H265),
        hdr && crate::encode::can_encode_10bit(Codec::Av1),
    )
}

/// Pure so tests can pin HDR layering without a GPU.
fn apply_hdr(base: u32, hevc_10bit: bool, av1_10bit: bool) -> u32 {
    let mut m = base;
    if hevc_10bit && base & super::SCM_HEVC != 0 {
        m |= super::SCM_HEVC_MAIN10;
    }
    if av1_10bit && base & super::SCM_AV1_MAIN8 != 0 {
        m |= super::SCM_AV1_MAIN10;
    }
    m
}

/// SDR baseline. Probe when the backend can, so Moonlight never negotiates a
/// codec the encoder cannot open; fail open to [`SERVER_CODEC_MODE_SUPPORT`].
/// HDR bits are layered by [`codec_mode_support`].
fn base_codec_mode_support() -> u32 {
    // Software encodes H.264 only. A local config read, not `host_wire_caps()`:
    // that re-runs DXGI enumeration per `/serverinfo` poll.
    if matches!(
        pf_host_config::config().encoder_pref.as_str(),
        "software" | "sw" | "openh264"
    ) {
        return super::SCM_H264;
    }
    // The same narrowed probe `host_wire_caps` advertises, so Moonlight is never
    // offered a codec the native path withholds. Fail-open: `probed_mask` is
    // None → the static superset below.
    #[cfg(target_os = "linux")]
    if let Some(m) = probed_mask(crate::encode::linux_advertised_codec_support()) {
        return m;
    }
    // AMF probes with no extra feature; QSV needs the `qsv` build and NVENC the
    // `nvenc` one. Unprobed → superset, same fail-open as the Linux arms.
    #[cfg(target_os = "windows")]
    if crate::encode::windows_backend_is_probed() {
        if let Some(m) = probed_mask(crate::encode::windows_codec_support()) {
            return m;
        }
    }
    SERVER_CODEC_MODE_SUPPORT
}

/// Map a probe to a `ServerCodecModeSupport` mask. `None` means the GPU was
/// unusable at probe time, not that it encodes nothing — the caller then
/// advertises the static superset rather than claiming zero codecs.
#[cfg(any(target_os = "linux", target_os = "windows"))]
fn probed_mask(caps: crate::encode::CodecSupport) -> Option<u32> {
    use super::{SCM_AV1_MAIN8, SCM_H264, SCM_HEVC};
    let mut m = 0;
    if caps.h264 {
        m |= SCM_H264;
    }
    if caps.h265 {
        m |= SCM_HEVC;
    }
    if caps.av1 {
        m |= SCM_AV1_MAIN8;
    }
    (m != 0).then_some(m)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gamestream::{SCM_AV1_MAIN10, SCM_AV1_MAIN8, SCM_H264, SCM_HEVC, SCM_HEVC_MAIN10};

    /// Static SDR superset (moonlight-common-c `Limelight.h`). No 10-bit bits:
    /// HDR is layered at advertise time, not baked into this constant.
    #[test]
    fn codec_mode_support_mask() {
        assert_eq!(SERVER_CODEC_MODE_SUPPORT, 0x1 | 0x100 | 0x10000);
        assert_eq!(SERVER_CODEC_MODE_SUPPORT, 65793);
        assert_eq!(
            SERVER_CODEC_MODE_SUPPORT & SCM_HEVC_MAIN10,
            0,
            "no 10-bit/HDR claim"
        );
        assert_eq!(
            SERVER_CODEC_MODE_SUPPORT,
            SCM_H264 | SCM_HEVC | SCM_AV1_MAIN8
        );
    }

    /// Each 10-bit bit needs encode capability and the SDR baseline bit. An
    /// over-claim invites the client into a mode the encoder cannot open.
    #[test]
    fn apply_hdr_adds_each_codecs_10bit_bit_independently() {
        let sdr = SCM_H264 | SCM_HEVC | SCM_AV1_MAIN8;
        assert_eq!(
            apply_hdr(sdr, true, true),
            sdr | SCM_HEVC_MAIN10 | SCM_AV1_MAIN10
        );
        assert_eq!(apply_hdr(sdr, false, false), sdr);
        assert_eq!(apply_hdr(sdr, true, false), sdr | SCM_HEVC_MAIN10);
        assert_eq!(apply_hdr(sdr, false, true), sdr | SCM_AV1_MAIN10);
        assert_eq!(apply_hdr(SCM_H264, true, true), SCM_H264);
        assert_eq!(
            apply_hdr(SCM_H264 | SCM_HEVC, true, true),
            SCM_H264 | SCM_HEVC | SCM_HEVC_MAIN10
        );
    }

    #[test]
    fn serverinfo_xml_carries_codec_mask() {
        let host = Host {
            hostname: "test".into(),
            uniqueid: "uid".into(),
            http_port: 47989,
            https_port: 47984,
            os_chain: "linux".into(),
            os_name: "Linux".into(),
        };
        let xml = serverinfo_xml(&host, false, false, 0, None);
        // Pin the XML to `codec_mode_support()`, not a literal: the mask is GPU-probed.
        let mask = codec_mode_support();
        assert!(mask != 0, "must advertise at least one codec");
        assert!(xml.contains(&format!(
            "<ServerCodecModeSupport>{mask}</ServerCodecModeSupport>"
        )));
    }

    #[test]
    fn serverinfo_xml_escapes_the_host_name() {
        let host = Host {
            hostname: "Tom & Jerry <3".into(),
            uniqueid: "uid".into(),
            http_port: 47989,
            https_port: 47984,
            os_chain: "linux".into(),
            os_name: "Linux".into(),
        };
        let xml = serverinfo_xml(&host, false, false, 0, None);
        assert!(
            xml.contains("<hostname>Tom &amp; Jerry &lt;3</hostname>"),
            "{xml}"
        );
    }

    /// Moonlight stores `LocalIP` (and the MAC beside it) for reconnects and Wake-on-LAN.
    #[test]
    fn serverinfo_reports_the_address_the_client_reached() {
        let host = Host {
            hostname: "test".into(),
            uniqueid: "uid".into(),
            http_port: 47989,
            https_port: 47984,
            os_chain: "linux".into(),
            os_name: "Linux".into(),
        };
        for reached in ["10.9.8.7", "::ffff:10.9.8.7"] {
            let xml = serverinfo_xml(&host, false, false, 0, Some(reached.parse().unwrap()));
            assert!(
                xml.contains("<LocalIP>10.9.8.7</LocalIP>"),
                "{reached}: {xml}"
            );
        }
    }

    /// Plain HTTP always zeros. HTTPS is the routed-NIC MAC or zeros, never
    /// `01:02:03:04:05:06` — Moonlight persists `<mac>` for Wake-on-LAN.
    #[test]
    fn mac_is_real_or_hidden_never_fake() {
        let host = Host {
            hostname: "test".into(),
            uniqueid: "uid".into(),
            http_port: 47989,
            https_port: 47984,
            os_chain: "linux".into(),
            os_name: "Linux".into(),
        };
        let http = serverinfo_xml(&host, false, false, 0, None);
        assert!(http.contains("<mac>00:00:00:00:00:00</mac>"));
        let https = serverinfo_xml(&host, true, true, 0, None);
        assert!(!https.contains("01:02:03:04:05:06"), "the fake MAC is gone");
        assert!(https.contains("<mac>"), "a mac element is always present");
    }

    /// Moonlight keys Resume/Quit on `currentgame` and `_SERVER_BUSY` moving together.
    #[test]
    fn serverinfo_busy_state_tracks_current_game() {
        let host = Host {
            hostname: "test".into(),
            uniqueid: "uid".into(),
            http_port: 47989,
            https_port: 47984,
            os_chain: "linux".into(),
            os_name: "Linux".into(),
        };
        let free = serverinfo_xml(&host, true, true, 0, None);
        assert!(free.contains("<currentgame>0</currentgame>"));
        assert!(free.contains("<state>SUNSHINE_SERVER_FREE</state>"));
        let busy = serverinfo_xml(&host, true, true, 881_448_767, None);
        assert!(busy.contains("<currentgame>881448767</currentgame>"));
        assert!(busy.contains("<state>SUNSHINE_SERVER_BUSY</state>"));
    }
}
