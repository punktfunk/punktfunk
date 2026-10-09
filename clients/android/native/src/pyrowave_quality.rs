//! JNI seam for the PyroWave quality row. Core prices the rate a quality needs and words the
//! warning when this device's link is short of it, so the row reads as it does on every client.

use jni::errors::LogErrorAndDefault;
use jni::objects::{JIntArray, JObject, JString};
use jni::sys::{jboolean, jint};
use jni::EnvUnowned;
use punktfunk_core::transport::LinkFacts;

/// `"<rate>\n<warning>"`: what `bpp_x100` needs at the mode as a player reads it, then the
/// warning for `link`, empty when the rate fits.
fn quality_lines(
    mode: punktfunk_core::Mode,
    chroma_444: bool,
    bit_depth: u8,
    bpp_x100: u16,
    link: LinkFacts,
) -> String {
    use punktfunk_core::pyrowave::{bpp_of_x100, kbps_for, link_warning, rate_label, BPP_DEFAULT};
    let bpp = bpp_of_x100(bpp_x100).unwrap_or(BPP_DEFAULT);
    let kbps = kbps_for(&mode, chroma_444, bit_depth, bpp);
    let warning = link_warning(kbps, link).unwrap_or_default();
    format!("{}\n{warning}", rate_label(kbps))
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
}
