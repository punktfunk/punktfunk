//! What the stream offers the producer: its probed traits, the pacing, and the dmabuf
//! modifier lists per format.

use super::map_format;
use crate::linux::pw_pods::{offer_framerate_denom, Pacing, HDR_FORMAT_ORDER};
use crate::{PixelFormat, ZeroCopyPolicy};
use pipewire as pw;
use pw::spa::param::video::VideoFormat;

/// The encoder-proved list for an exact drm fourcc out of
/// [`ZeroCopyPolicy::encoder_modifiers`]: cloned, LINEAR stripped, order kept.
fn encoder_modifiers_for(policy: &ZeroCopyPolicy, fourcc: u32) -> Vec<u64> {
    let Some(list) = policy
        .encoder_modifiers
        .iter()
        .find(|(f, _)| *f == fourcc)
        .map(|(_, m)| m)
    else {
        return Vec::new();
    };
    let mut out: Vec<u64> = Vec::with_capacity(list.len());
    for &m in list {
        if m != 0 && !out.contains(&m) {
            out.push(m);
        }
    }
    out
}

/// What the producer node says about itself before the stream connects. The registry
/// announce carries only a subset of a node's props, so the node is bound and read.
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct ProducerProbe {
    /// It emits RequestProcess (`node.supports-request` > 0). False on any doubt — a wrong
    /// true makes a non-lazy producer a follower of a driver that never triggers.
    pub(super) supports_request: bool,
    /// Its `EnumFormat` states `maxFramerate` in millihertz: KWin 6.8+, which paces the
    /// cast at the ceiling that fixates. Older KWin offers whole hertz and throttles.
    pub(super) framerate_mhz: bool,
}

pub(super) fn probe_producer(
    core: &pw::core::CoreRc,
    mainloop: &pw::main_loop::MainLoopRc,
    node_id: u32,
) -> ProducerProbe {
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;
    let Ok(registry) = core.get_registry_rc() else {
        return ProducerProbe::default();
    };
    let found: Rc<Cell<Option<bool>>> = Rc::new(Cell::new(None));
    let mhz: Rc<Cell<Option<bool>>> = Rc::new(Cell::new(None));
    // The bound proxy and its listener must outlive the round trips that deliver `info`.
    let bound: Rc<RefCell<Option<(pw::node::Node, pw::node::NodeListener)>>> = Rc::default();
    let _reg = registry
        .add_listener_local()
        .global({
            let (registry, found, mhz, bound) =
                (registry.clone(), found.clone(), mhz.clone(), bound.clone());
            move |g| {
                if g.id != node_id || g.type_ != pw::types::ObjectType::Node {
                    return;
                }
                let Ok(node) = registry.bind::<pw::node::Node, _>(g) else {
                    found.set(Some(false));
                    return;
                };
                let listener = node
                    .add_listener_local()
                    .info({
                        let found = found.clone();
                        move |info| {
                            let v = info
                                .props()
                                .and_then(|p| p.get("node.supports-request"))
                                .and_then(|v| v.trim().parse::<u32>().ok())
                                .unwrap_or(0);
                            found.set(Some(v > 0));
                        }
                    })
                    .param({
                        let mhz = mhz.clone();
                        move |_, id, _, _, pod| {
                            if id != pw::spa::param::ParamType::EnumFormat {
                                return;
                            }
                            if let Some(denom) =
                                pod.and_then(|p| offer_framerate_denom(p.as_bytes()))
                            {
                                mhz.set(Some(denom == 1000));
                            }
                        }
                    })
                    .register();
                node.enum_params(0, Some(pw::spa::param::ParamType::EnumFormat), 0, u32::MAX);
                *bound.borrow_mut() = Some((node, listener));
            }
        })
        .register();
    // Round 1 replays the globals and binds; round 2 lands the bind's `info` and formats.
    // The timer bounds a daemon that never answers.
    let awaited: Rc<Cell<Option<pw::spa::utils::result::AsyncSeq>>> = Rc::new(Cell::new(None));
    let _core_l = core
        .add_listener_local()
        .done({
            let (ml, awaited) = (mainloop.clone(), awaited.clone());
            move |_, seq| {
                if awaited.get() == Some(seq) {
                    ml.quit();
                }
            }
        })
        .register();
    let guard = mainloop.loop_().add_timer({
        let ml = mainloop.clone();
        move |_| ml.quit()
    });
    let _ = guard.update_timer(Some(std::time::Duration::from_secs(2)), None);
    for _ in 0..3 {
        let Ok(seq) = core.sync(0) else {
            return ProducerProbe::default();
        };
        awaited.set(Some(seq));
        mainloop.run();
        if found.get().is_some() && mhz.get().is_some() {
            break;
        }
    }
    let (supports, framerate_mhz) = (found.get(), mhz.get());
    tracing::info!(
        node_id,
        supports_request = ?supports,
        framerate_mhz = ?framerate_mhz,
        "capture producer probed"
    );
    ProducerProbe {
        supports_request: supports.unwrap_or(false),
        framerate_mhz: framerate_mhz.unwrap_or(false),
    }
}

/// The offer's `maxFramerate`. KWin up to 6.7 asks for its own damage signal: it takes no
/// ceiling above its refresh, and that one it throttles with a whole-millisecond timer that
/// slips a frame every few. KWin 6.8 (millihertz offer) paces the cast at the ceiling, so
/// it gets the stream rate. gamescope paints on every commit, so the wire rate caps its
/// pushes; anyone else keeps its rate.
pub(super) fn offer_pacing(
    unpaced: bool,
    producer_mhz: bool,
    gamescope: bool,
    preferred: Option<(u32, u32, u32)>,
) -> Pacing {
    let hz = preferred.map(|(_, _, hz)| hz).filter(|hz| *hz > 0);
    if unpaced {
        if producer_mhz {
            hz.map_or(Pacing::Unpaced, Pacing::Cap)
        } else {
            Pacing::Unpaced
        }
    } else if gamescope {
        hz.map_or(Pacing::Producer, Pacing::Cap)
    } else {
        Pacing::Producer
    }
}

/// BGRx/BGRA dmabuf offers. Importer lists per format; the direct-import lane's
/// tiled seed is `ZeroCopyPolicy::encoder_modifiers`, offered only to a
/// tiled-opted gamescope producer on VA passthrough while the refusal latch is
/// clear; PyroWave merges its Vulkan-importable list on non-gamescope
/// passthrough. `dmabuf_modifiers_for_producer` finalizes each list. Returns
/// `(bgrx, bgra, extend_pyrowave)` — the flag feeds the session-start log line.
pub(super) fn packed_modifier_offers(
    policy: &ZeroCopyPolicy,
    health: &pf_zerocopy::ZeroCopyHealth,
    importer: Option<&mut pf_zerocopy::Importer>,
    vaapi_passthrough: bool,
    producer_is_gamescope: bool,
) -> (Vec<u64>, Vec<u64>, bool) {
    // EGL importer answers per format; the encoder seed does too. LINEAR is appended for
    // every advertised list and remains the only gamescope choice without a proved tiled seed.
    let advertise = importer.is_some() || vaapi_passthrough;
    let mut modifiers = Vec::new();
    let mut modifiers_bgra = Vec::new();
    if let Some(i) = importer {
        modifiers = i.supported_modifiers(pf_frame::drm_fourcc(PixelFormat::Bgrx).unwrap());
        modifiers_bgra = i.supported_modifiers(pf_frame::drm_fourcc(PixelFormat::Bgra).unwrap());
    }
    // PyroWave imports through Vulkan, not libva. Its per-fourcc lists come from the
    // facade so capture never calls `encode`; gamescope has its separately gated seed.
    let extend_pyrowave = vaapi_passthrough && policy.pyrowave_session && !producer_is_gamescope;
    // The direct-import lane's tiled offer comes from what the session encoder
    // proved (`ZeroCopyPolicy::encoder_modifiers`), per fourcc. A refused tiled
    // offer or a non-gamescope producer keeps the importer's list alone.
    let tiled_refused = health.passthrough_tiled_refused();
    let seed_encoder_mods =
        vaapi_passthrough && producer_is_gamescope && policy.gamescope_tiled && !tiled_refused;
    for (fourcc, mods) in &policy.encoder_modifiers {
        let nonzero: Vec<u64> = mods.iter().copied().filter(|m| *m != 0).collect();
        if !nonzero.is_empty() {
            tracing::info!(
                fourcc = format!("{fourcc:#010x}"),
                modifiers = ?nonzero,
                "zero-copy: encoder-proved tiled dmabuf modifiers for capture"
            );
        }
    }
    for (list, fmt) in [
        (&mut modifiers, PixelFormat::Bgrx),
        (&mut modifiers_bgra, PixelFormat::Bgra),
    ] {
        if seed_encoder_mods || extend_pyrowave {
            if let Some(fourcc) = pf_frame::drm_fourcc(fmt) {
                for m in encoder_modifiers_for(policy, fourcc) {
                    if !list.contains(&m) {
                        list.push(m);
                    }
                }
            }
        }
        *list = dmabuf_modifiers_for_producer(
            list,
            advertise,
            producer_is_gamescope
                && (!policy.gamescope_tiled || (vaapi_passthrough && tiled_refused)),
        );
    }
    (modifiers, modifiers_bgra, extend_pyrowave)
}

/// Packed 10-bit offers, PQ or gamescope's SDR (`want_ten_bit`). Tiled has two readers:
/// NVENC's raw convert, and the VA encoder's own import — the latter only for
/// `encoder_modifiers`-proved modifiers. Every other arm de-tiles into 8 bits, so tiled is
/// offered only while one lane holds the stream. LINEAR is always appended once.
pub(super) fn hdr_modifier_offers(
    policy: &ZeroCopyPolicy,
    health: &pf_zerocopy::ZeroCopyHealth,
    importer: Option<&mut pf_zerocopy::Importer>,
    want_ten_bit: bool,
    vaapi_passthrough: bool,
    producer_is_gamescope: bool,
    nvenc_raw: bool,
) -> Vec<(VideoFormat, Vec<u64>)> {
    let mut importer = importer;
    let hdr_tiled_raw =
        want_ten_bit && policy.gamescope_tiled && nvenc_raw && !health.hdr_tiled_refused();
    let hdr_tiled_direct = want_ten_bit
        && vaapi_passthrough
        && producer_is_gamescope
        && policy.gamescope_tiled
        && !health.passthrough_tiled_refused();
    let mut hdr_modifiers: Vec<(VideoFormat, Vec<u64>)> = Vec::new();
    for fmt in HDR_FORMAT_ORDER {
        let mut list = Vec::new();
        if hdr_tiled_direct {
            if let Some(fourcc) = map_format(fmt).and_then(pf_frame::drm_fourcc) {
                list = encoder_modifiers_for(policy, fourcc);
            }
        } else if hdr_tiled_raw {
            if let (Some(i), Some(fourcc)) = (
                importer.as_deref_mut(),
                map_format(fmt).and_then(pf_frame::drm_fourcc),
            ) {
                list = i.supported_modifiers(fourcc);
            }
        }
        list.retain(|&m| m != 0);
        list.push(0);
        hdr_modifiers.push((fmt, list));
    }
    hdr_modifiers
}

/// A `linear_only` gamescope node offers LINEAR as `{0,0}`. spa_pod_filter without
/// DONT_FIXATE fixates our default, so a tiled NVIDIA default fails the link. Empty `egl`
/// with `advertise` still yields LINEAR — the importer exists, EGL listed none.
fn dmabuf_modifiers_for_producer(egl: &[u64], advertise: bool, linear_only: bool) -> Vec<u64> {
    if !advertise {
        return egl.to_vec();
    }
    if linear_only {
        return vec![0];
    }
    let mut m = egl.to_vec();
    if !m.contains(&0) {
        m.push(0);
    }
    m
}

#[cfg(test)]
mod tests {
    use super::{dmabuf_modifiers_for_producer, encoder_modifiers_for, offer_pacing, Pacing};
    use crate::ZeroCopyPolicy;

    /// gamescope and KWin 6.8 are capped at the wire rate, older KWin never; a missing
    /// rate caps nothing.
    #[test]
    fn pacing_caps_follow_the_wire_rate() {
        assert_eq!(
            offer_pacing(true, false, false, Some((1, 1, 90))),
            Pacing::Unpaced
        );
        assert_eq!(
            offer_pacing(true, true, false, Some((1, 1, 90))),
            Pacing::Cap(90)
        );
        assert_eq!(
            offer_pacing(true, true, false, Some((1, 1, 0))),
            Pacing::Unpaced
        );
        assert_eq!(offer_pacing(true, true, false, None), Pacing::Unpaced);
        assert_eq!(
            offer_pacing(false, false, true, Some((1, 1, 90))),
            Pacing::Cap(90)
        );
        assert_eq!(
            offer_pacing(false, false, true, Some((1, 1, 0))),
            Pacing::Producer
        );
        assert_eq!(
            offer_pacing(false, false, false, Some((1, 1, 90))),
            Pacing::Producer
        );
    }

    /// NVIDIA block-linear, the EGL default on this host. gamescope does not offer it.
    const NVIDIA_TILED: u64 = 216172782120099856;

    #[test]
    fn gamescope_dmabuf_offer_fixates_linear() {
        let egl = [NVIDIA_TILED, NVIDIA_TILED + 4];
        assert_eq!(
            dmabuf_modifiers_for_producer(&egl, true, true),
            vec![0],
            "a forced LINEAR offer must discard tiled defaults"
        );
        assert_eq!(
            dmabuf_modifiers_for_producer(&[], true, true),
            vec![0],
            "a live importer with no EGL list still advertises LINEAR"
        );
        let kwin = dmabuf_modifiers_for_producer(&egl, true, false);
        assert_eq!(
            kwin[0], NVIDIA_TILED,
            "KWin lists the tiled mods; keep them first"
        );
        assert!(kwin.contains(&0));
        assert!(dmabuf_modifiers_for_producer(&[], false, true).is_empty());
    }

    /// The lookup clones only the exact fourcc's list, drops LINEAR entries and
    /// duplicates, and answers empty for a fourcc the encoder never proved.
    #[test]
    fn encoder_modifiers_lookup_is_exact_and_linear_stripped() {
        const XR24: u32 = 0x34325258;
        const AR24: u32 = 0x34325241;
        let policy = ZeroCopyPolicy {
            encoder_modifiers: vec![(
                XR24,
                vec![0x100000000000001, 0, 0x100000000000001, 0x100000000000002],
            )],
            ..Default::default()
        };
        assert_eq!(
            encoder_modifiers_for(&policy, XR24),
            vec![0x100000000000001, 0x100000000000002]
        );
        assert!(encoder_modifiers_for(&policy, AR24).is_empty());
        assert!(encoder_modifiers_for(&policy, 0xdeadbeef).is_empty());
    }
}
