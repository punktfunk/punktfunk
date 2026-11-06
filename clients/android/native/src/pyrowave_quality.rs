//! JNI seam for the PyroWave quality row. Core prices the rate a quality needs and names the
//! shortfall of this device's link; the messages word both, so the row reads as it does on every
//! client.

use jni::errors::LogErrorAndDefault;
use jni::objects::{JIntArray, JObject, JString};
use jni::sys::{jboolean, jint};
use jni::EnvUnowned;
use punktfunk_core::pyrowave::{
    bpp_of_x100, kbps_for, link_shortfall, rate, LinkShortfall, Rate, BPP_DEFAULT,
};
use punktfunk_core::transport::LinkFacts;

/// `kbps` as a player reads it: `940 Mbit/s`, `1.3 Gbit/s`.
fn rate_text(kbps: u32) -> String {
    match rate(kbps) {
        Rate::Mbit(n) => pf_l10n::common::common_mbits(n),
        Rate::Gbit(n) => pf_l10n::common::common_gbits(n),
    }
}

/// What to tell a player whose quality needs `kbps` over `link`; empty when it fits.
fn warning_text(kbps: u32, link: LinkFacts) -> String {
    use pf_l10n::client as m;
    match link_shortfall(kbps, link) {
        None => String::new(),
        Some(LinkShortfall::Wifi) => m::player_pyrowave_bpp_wifi_warning().into(),
        Some(LinkShortfall::Port(mbps)) => m::player_pyrowave_bpp_port_warning(
            rate_text(kbps),
            rate_text(mbps.saturating_mul(1000)),
        ),
        Some(LinkShortfall::Gbe) => m::player_pyrowave_bpp_gbe_warning(rate_text(kbps)),
    }
}

/// `"<rate>\n<warning>"`: what `bpp_x100` needs at the mode as a player reads it, then the
/// warning for `link`, empty when the rate fits.
fn quality_lines(
    mode: punktfunk_core::Mode,
    chroma_444: bool,
    bit_depth: u8,
    bpp_x100: u16,
    link: LinkFacts,
) -> String {
    let bpp = bpp_of_x100(bpp_x100).unwrap_or(BPP_DEFAULT);
    let kbps = kbps_for(&mode, chroma_444, bit_depth, bpp);
    format!("{}\n{}", rate_text(kbps), warning_text(kbps, link))
}

/// `NativeBridge.nativeLocalLink(): IntArray`: `[kind, mbps]` of this device's link toward its
/// default route, `IFACE_KIND_*` and Mbit/s. A route lookup and a few file reads: nothing is sent.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeLocalLink<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
) -> JIntArray<'local> {
    let f = punktfunk_core::transport::ifinfo::local_link_facts(None);
    let buf = [
        jint::from(f.kind),
        jint::try_from(f.mbps).unwrap_or(jint::MAX),
    ];
    env.with_env(|env| -> jni::errors::Result<JIntArray<'local>> {
        let arr = env.new_int_array(buf.len())?;
        arr.set_region(env, 0, &buf)?;
        Ok(arr)
    })
    .resolve::<LogErrorAndDefault>()
}

/// `NativeBridge.nativePyrowaveQuality(width, height, refreshHz, chroma444, bitDepth, bppX100,
/// linkKind, linkMbps): String`: [`quality_lines`] for that link.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativePyrowaveQuality<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    width: jint,
    height: jint,
    refresh_hz: jint,
    chroma_444: jboolean,
    bit_depth: jint,
    bpp_x100: jint,
    link_kind: jint,
    link_mbps: jint,
) -> JString<'local> {
    let px = |v: jint| u32::try_from(v).unwrap_or(0);
    let mode = punktfunk_core::Mode {
        width: px(width),
        height: px(height),
        refresh_hz: px(refresh_hz),
    };
    let link = LinkFacts {
        kind: u8::try_from(link_kind).unwrap_or(0),
        mbps: px(link_mbps),
    };
    let depth = u8::try_from(bit_depth).unwrap_or(8);
    let x100 = u16::try_from(bpp_x100).unwrap_or(0);
    let out = quality_lines(mode, chroma_444, depth, x100, link);
    env.with_env(|env| env.new_string(out))
        .resolve::<LogErrorAndDefault>()
}

#[cfg(test)]
mod tests {
    use super::*;
    use punktfunk_core::transport::{IFACE_KIND_ETHERNET, IFACE_KIND_WIFI};

    #[test]
    fn the_row_reads_as_core_prices_it() {
        let mode = punktfunk_core::Mode {
            width: 3840,
            height: 2160,
            refresh_hz: 120,
        };
        let wire = LinkFacts {
            kind: IFACE_KIND_ETHERNET,
            mbps: 2500,
        };
        assert_eq!(quality_lines(mode, false, 8, 160, wire), "1.6 Gbit/s\n");
        let wifi = LinkFacts {
            kind: IFACE_KIND_WIFI,
            mbps: 0,
        };
        assert_eq!(
            quality_lines(mode, false, 8, 160, wifi),
            "1.6 Gbit/s\nPyroWave needs a wired link; this device is on Wi-Fi."
        );
    }

    /// Both lines come from messages: the pseudo-language brackets each of them.
    #[test]
    fn the_row_is_worded_from_messages() {
        let mode = punktfunk_core::Mode {
            width: 1920,
            height: 1080,
            refresh_hz: 60,
        };
        let wifi = LinkFacts {
            kind: IFACE_KIND_WIFI,
            mbps: 0,
        };
        let lines = pf_l10n::with_language("en-XA", || quality_lines(mode, false, 8, 160, wifi));
        let (rate, warning) = lines.split_once('\n').unwrap();
        assert!(rate.starts_with('[') && warning.starts_with('['), "{lines}");
    }
}
