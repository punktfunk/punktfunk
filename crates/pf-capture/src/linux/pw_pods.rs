//! PipeWire `EnumFormat` / `Buffers` / `Meta` pods the capture stream offers.
//!
//! These builders are the crate's wire surface: the compositor intersects
//! whatever they put in a pod. A missing property is not a compile error; it
//! stalls the link in `negotiating`. Pin the offers with the tests in this
//! module. Every function is `facts -> Vec<u8>` — nothing here owns the stream,
//! the buffers, or the frames.

use anyhow::{Context, Result};
use pipewire as pw;
use pw::spa;
use spa::param::video::VideoFormat;

use super::sync_timeline::{MetaSyncTimeline, META_SYNC_TIMELINE, PARAM_BUFFERS_META_TYPE};

pub(super) fn serialize_pod(obj: pw::spa::pod::Object) -> Result<Vec<u8>> {
    Ok(pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(obj),
    )
    .context("serialize pod")?
    .0
    .into_inner())
}

/// What the offer's `maxFramerate` tells the producer.
///
/// `Unpaced` is `0/1`: no ceiling, so KWin delivers on its own damage signal. Up to 6.7
/// the only ceiling KWin takes is its refresh (`Range(refresh, 0/1, refresh)`), and it
/// throttles that with a whole-millisecond timer stamped after each render: at the
/// stream rate every record slips a millisecond until one coalesces, ~9.3 ms apart.
/// KWin 6.8 offers millihertz and paces the cast at the ceiling that fixates, so there
/// the stream rate goes on as `Cap`.
///
/// `Cap(hz)` is the wire rate, for a producer that paints on every commit (gamescope
/// with adaptive sync): it pushes at most `hz` frames a second. A producer that never
/// read the property is unchanged by it. `Producer` sends none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Pacing {
    Producer,
    Unpaced,
    Cap(u32),
}

fn max_framerate_prop(pacing: Pacing) -> Option<pw::spa::pod::Property> {
    let num = match pacing {
        Pacing::Producer => return None,
        Pacing::Unpaced => 0,
        Pacing::Cap(hz) => hz,
    };
    Some(pw::spa::pod::Property {
        key: pw::spa::sys::SPA_FORMAT_VIDEO_maxFramerate,
        flags: pw::spa::pod::PropertyFlags::empty(),
        value: pw::spa::pod::Value::Fraction(pw::spa::utils::Fraction { num, denom: 1 }),
    })
}

/// NV12 also pins BT.709 limited; packed RGB must not — it is not YUV.
pub(super) fn build_dmabuf_format(
    format: VideoFormat,
    modifiers: &[u64],
    preferred: Option<(u32, u32, u32)>,
    pacing: Pacing,
) -> Result<Vec<u8>> {
    let (dw, dh, dhz) = preferred.unwrap_or((1920, 1080, 60));
    use pw::spa::param::format::{FormatProperties, MediaSubtype, MediaType};
    let mut obj = pw::spa::pod::object!(
        pw::spa::utils::SpaTypes::ObjectParamFormat,
        pw::spa::param::ParamType::EnumFormat,
        pw::spa::pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
        pw::spa::pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        pw::spa::pod::property!(FormatProperties::VideoFormat, Id, format),
        pw::spa::pod::property!(
            FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            pw::spa::utils::Rectangle {
                width: dw,
                height: dh
            },
            pw::spa::utils::Rectangle {
                width: 1,
                height: 1
            },
            pw::spa::utils::Rectangle {
                width: 8192,
                height: 8192
            }
        ),
        pw::spa::pod::property!(
            FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            pw::spa::utils::Fraction { num: dhz, denom: 1 },
            pw::spa::utils::Fraction { num: 0, denom: 1 },
            pw::spa::utils::Fraction { num: 240, denom: 1 }
        ),
    );
    if format == VideoFormat::NV12 {
        obj.properties.push(pw::spa::pod::Property {
            key: pw::spa::sys::SPA_FORMAT_VIDEO_colorMatrix,
            flags: pw::spa::pod::PropertyFlags::MANDATORY,
            value: pw::spa::pod::Value::Id(pw::spa::utils::Id(
                pw::spa::sys::SPA_VIDEO_COLOR_MATRIX_BT709,
            )),
        });
        obj.properties.push(pw::spa::pod::Property {
            key: pw::spa::sys::SPA_FORMAT_VIDEO_colorRange,
            flags: pw::spa::pod::PropertyFlags::MANDATORY,
            value: pw::spa::pod::Value::Id(pw::spa::utils::Id(
                pw::spa::sys::SPA_VIDEO_COLOR_RANGE_16_235,
            )),
        });
    }
    if let Some(p) = max_framerate_prop(pacing) {
        obj.properties.push(p);
    }
    obj.properties.push(pw::spa::pod::Property {
        key: pw::spa::sys::SPA_FORMAT_VIDEO_modifier,
        flags: pw::spa::pod::PropertyFlags::MANDATORY,
        value: pw::spa::pod::Value::Choice(pw::spa::pod::ChoiceValue::Long(
            pw::spa::utils::Choice(
                pw::spa::utils::ChoiceFlags::empty(),
                pw::spa::utils::ChoiceEnum::Enum {
                    default: modifiers[0] as i64,
                    alternatives: modifiers.iter().map(|&m| m as i64).collect(),
                },
            ),
        )),
    });
    serialize_pod(obj)
}

/// PQ (`SPA_VIDEO_TRANSFER_SMPTE2084`). 14 is the wire ABI in
/// `spa/param/video/color.h` (same index as GStreamer's
/// `GstVideoTransferFunction`). Not taken from `pw::spa::sys` because older
/// distro headers omit the symbol and bindgen then fails the host compile.
/// A libspa without it fails to intersect this offer and the session stays SDR.
const SPA_VIDEO_TRANSFER_SMPTE2084: u32 = 14;

/// 10-bit PQ formats in negotiation order. The first compatible consumer pod
/// wins, so this is colour correctness, not style: NVIDIA does not implement
/// linear-tiled `A2R10G10B10`, and gamescope's mappable capture then writes
/// XBGR bytes under an `XRGB2101010` label. Host mappings all look right, so
/// nothing downstream can detect the swap.
///
/// `xRGB_210LE` stays second so a producer that offers only it can still
/// negotiate HDR instead of dropping to SDR. Do not reorder.
pub(super) const HDR_FORMAT_ORDER: [VideoFormat; 2] =
    [VideoFormat::xBGR_210LE, VideoFormat::xRGB_210LE];

/// 10-bit PQ `EnumFormat`. `modifiers` is `[0]` unless the raw lane takes this stream: the
/// EGL de-tile blit renders into `GL_RGBA8` and would crush the depth. The modifier is a
/// Choice enum (same shape as [`build_dmabuf_format`]): a scalar `Long(0)` does not
/// intersect gamescope's `{default:0, alt:0}` choice.
/// BT.2020 + PQ are **MANDATORY** — Mutter's HDR pods are too.
pub(super) fn build_hdr_dmabuf_format(
    format: VideoFormat,
    modifiers: &[u64],
    preferred: Option<(u32, u32, u32)>,
    pacing: Pacing,
) -> Result<Vec<u8>> {
    let (dw, dh, dhz) = preferred.unwrap_or((1920, 1080, 60));
    use pw::spa::param::format::{FormatProperties, MediaSubtype, MediaType};
    let mut obj = pw::spa::pod::object!(
        pw::spa::utils::SpaTypes::ObjectParamFormat,
        pw::spa::param::ParamType::EnumFormat,
        pw::spa::pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
        pw::spa::pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        pw::spa::pod::property!(FormatProperties::VideoFormat, Id, format),
        pw::spa::pod::property!(
            FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            pw::spa::utils::Rectangle {
                width: dw,
                height: dh
            },
            pw::spa::utils::Rectangle {
                width: 1,
                height: 1
            },
            pw::spa::utils::Rectangle {
                width: 8192,
                height: 8192
            }
        ),
        pw::spa::pod::property!(
            FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            pw::spa::utils::Fraction { num: dhz, denom: 1 },
            pw::spa::utils::Fraction { num: 0, denom: 1 },
            pw::spa::utils::Fraction { num: 240, denom: 1 }
        ),
    );
    if let Some(p) = max_framerate_prop(pacing) {
        obj.properties.push(p);
    }
    // P010 is YUV: pin the matrix the bitstream declares, as the NV12 offer does.
    if format == VideoFormat::P010_10LE {
        obj.properties.push(pw::spa::pod::Property {
            key: pw::spa::sys::SPA_FORMAT_VIDEO_colorMatrix,
            flags: pw::spa::pod::PropertyFlags::MANDATORY,
            value: pw::spa::pod::Value::Id(pw::spa::utils::Id(
                pw::spa::sys::SPA_VIDEO_COLOR_MATRIX_BT2020,
            )),
        });
        obj.properties.push(pw::spa::pod::Property {
            key: pw::spa::sys::SPA_FORMAT_VIDEO_colorRange,
            flags: pw::spa::pod::PropertyFlags::MANDATORY,
            value: pw::spa::pod::Value::Id(pw::spa::utils::Id(
                pw::spa::sys::SPA_VIDEO_COLOR_RANGE_16_235,
            )),
        });
    }
    obj.properties.push(pw::spa::pod::Property {
        key: pw::spa::sys::SPA_FORMAT_VIDEO_modifier,
        flags: pw::spa::pod::PropertyFlags::MANDATORY,
        value: pw::spa::pod::Value::Choice(pw::spa::pod::ChoiceValue::Long(
            pw::spa::utils::Choice(
                pw::spa::utils::ChoiceFlags::empty(),
                pw::spa::utils::ChoiceEnum::Enum {
                    default: modifiers[0] as i64,
                    alternatives: modifiers.iter().map(|&m| m as i64).collect(),
                },
            ),
        )),
    });
    obj.properties.push(pw::spa::pod::Property {
        key: pw::spa::sys::SPA_FORMAT_VIDEO_transferFunction,
        flags: pw::spa::pod::PropertyFlags::MANDATORY,
        value: pw::spa::pod::Value::Id(pw::spa::utils::Id(SPA_VIDEO_TRANSFER_SMPTE2084)),
    });
    obj.properties.push(pw::spa::pod::Property {
        key: pw::spa::sys::SPA_FORMAT_VIDEO_colorPrimaries,
        flags: pw::spa::pod::PropertyFlags::MANDATORY,
        value: pw::spa::pod::Value::Id(pw::spa::utils::Id(
            pw::spa::sys::SPA_VIDEO_COLOR_PRIMARIES_BT2020,
        )),
    });
    serialize_pod(obj)
}

/// SHM/CPU `EnumFormat`. Framerate 0/1 is variable; gamescope fixates that.
pub(super) fn build_default_format_obj(
    preferred: Option<(u32, u32, u32)>,
    pacing: Pacing,
) -> pw::spa::pod::Object {
    let (dw, dh, dhz) = preferred.unwrap_or((1920, 1080, 60));
    let mut obj = pw::spa::pod::object!(
        pw::spa::utils::SpaTypes::ObjectParamFormat,
        pw::spa::param::ParamType::EnumFormat,
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::MediaType,
            Id,
            pw::spa::param::format::MediaType::Video
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::MediaSubtype,
            Id,
            pw::spa::param::format::MediaSubtype::Raw
        ),
        // Encoder-mappable layouts only. wlroots often fixates packed RGB
        // (3 bpp); others offer 4 bpp. Restricting the enum fails loudly
        // instead of handing us a format we would misinterpret.
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            VideoFormat::RGB,
            VideoFormat::RGB,
            VideoFormat::BGR,
            VideoFormat::RGBx,
            VideoFormat::BGRx,
            VideoFormat::RGBA,
            VideoFormat::BGRA,
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            pw::spa::utils::Rectangle {
                width: dw,
                height: dh
            },
            pw::spa::utils::Rectangle {
                width: 1,
                height: 1
            },
            pw::spa::utils::Rectangle {
                width: 8192,
                height: 8192
            }
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            pw::spa::utils::Fraction { num: dhz, denom: 1 },
            pw::spa::utils::Fraction { num: 0, denom: 1 },
            pw::spa::utils::Fraction { num: 240, denom: 1 }
        ),
    );
    if let Some(p) = max_framerate_prop(pacing) {
        obj.properties.push(p);
    }
    obj
}

/// CPU-path Buffers: MemPtr, MemFd, and DmaBuf. Gamescope's modifier-bearing
/// (LINEAR) pod then offers *only* DmaBuf; without that bit the type
/// intersection is empty and the link stalls in `negotiating`. A LINEAR
/// dmabuf is mmap-able, so the CPU de-pad copy still works.
pub(super) fn build_mappable_buffers() -> Result<Vec<u8>> {
    serialize_pod(pw::spa::pod::Object {
        type_: pw::spa::utils::SpaTypes::ObjectParamBuffers.as_raw(),
        id: pw::spa::param::ParamType::Buffers.as_raw(),
        properties: vec![pw::spa::pod::Property {
            key: pw::spa::sys::SPA_PARAM_BUFFERS_dataType,
            flags: pw::spa::pod::PropertyFlags::empty(),
            value: pw::spa::pod::Value::Int(
                (1i32 << pw::spa::sys::SPA_DATA_MemPtr)
                    | (1i32 << pw::spa::sys::SPA_DATA_MemFd)
                    | (1i32 << pw::spa::sys::SPA_DATA_DmaBuf),
            ),
        }],
    })
}

/// SHM Buffers: MemPtr + MemFd, no DmaBuf. Mutter on NVIDIA renders into the
/// pool with no implicit dmabuf fence and no explicit sync_fd, so any dmabuf
/// read races the render and flashes the previous frame. Excluding DmaBuf
/// forces `glReadPixels` into mappable memory, which orders against render.
pub(super) fn build_shm_only_buffers() -> Result<Vec<u8>> {
    serialize_pod(pw::spa::pod::Object {
        type_: pw::spa::utils::SpaTypes::ObjectParamBuffers.as_raw(),
        id: pw::spa::param::ParamType::Buffers.as_raw(),
        properties: vec![pw::spa::pod::Property {
            key: pw::spa::sys::SPA_PARAM_BUFFERS_dataType,
            flags: pw::spa::pod::PropertyFlags::empty(),
            value: pw::spa::pod::Value::Int(
                (1i32 << pw::spa::sys::SPA_DATA_MemPtr) | (1i32 << pw::spa::sys::SPA_DATA_MemFd),
            ),
        }],
    })
}

/// Zero-copy pool depth, as a Choice range. A fixed count a producer cannot
/// afford empties the Buffers intersection and the link stalls in
/// `negotiating` (same trap as `build_cursor_meta_param`'s size).
///
/// `pool_min` is [`crate::POOL_MIN`], or [`crate::KWIN_POOL_MIN`] for KWin.
/// `default` 8 is ~133 ms at 60 Hz / ~33 ms at 240 Hz, past capture→fence
/// latency with a second frame in flight. `max` 16 caps compositor RAM
/// (~25 MB per 4K 4:4:4 buffer).
const POOL_DEFAULT: i32 = 8;
const POOL_MAX: i32 = 16;

/// `explicit_sync` adds a MANDATORY `metaType` naming `SPA_META_SyncTimeline`: a producer
/// without the meta fails this pod and takes the plain twin listed behind it, instead of a
/// pool this side would wait on for fences that never come.
pub(super) fn build_dmabuf_buffers(pool_min: i32, explicit_sync: bool) -> Result<Vec<u8>> {
    let mut properties = vec![
        pw::spa::pod::Property {
            key: pw::spa::sys::SPA_PARAM_BUFFERS_dataType,
            flags: pw::spa::pod::PropertyFlags::empty(),
            value: pw::spa::pod::Value::Int(1i32 << pw::spa::sys::SPA_DATA_DmaBuf),
        },
        pw::spa::pod::Property {
            key: pw::spa::sys::SPA_PARAM_BUFFERS_buffers,
            flags: pw::spa::pod::PropertyFlags::empty(),
            value: pw::spa::pod::Value::Choice(pw::spa::pod::ChoiceValue::Int(
                pw::spa::utils::Choice(
                    pw::spa::utils::ChoiceFlags::empty(),
                    pw::spa::utils::ChoiceEnum::Range {
                        default: POOL_DEFAULT,
                        min: pool_min,
                        max: POOL_MAX,
                    },
                ),
            )),
        },
    ];
    if explicit_sync {
        properties.push(pw::spa::pod::Property {
            key: PARAM_BUFFERS_META_TYPE,
            flags: pw::spa::pod::PropertyFlags::MANDATORY,
            value: pw::spa::pod::Value::Int(1i32 << META_SYNC_TIMELINE),
        });
    }
    serialize_pod(pw::spa::pod::Object {
        type_: pw::spa::utils::SpaTypes::ObjectParamBuffers.as_raw(),
        id: pw::spa::param::ParamType::Buffers.as_raw(),
        properties,
    })
}

/// `SPA_META_SyncTimeline` on each buffer. PipeWire adds it, and the two syncobj datas the
/// Buffers pod's `metaType` names, only when both sides list this meta.
pub(super) fn build_sync_timeline_meta_param() -> Result<Vec<u8>> {
    serialize_pod(pw::spa::pod::Object {
        type_: pw::spa::utils::SpaTypes::ObjectParamMeta.as_raw(),
        id: pw::spa::param::ParamType::Meta.as_raw(),
        properties: vec![
            pw::spa::pod::Property {
                key: pw::spa::sys::SPA_PARAM_META_type,
                flags: pw::spa::pod::PropertyFlags::empty(),
                value: pw::spa::pod::Value::Id(pw::spa::utils::Id(META_SYNC_TIMELINE)),
            },
            pw::spa::pod::Property {
                key: pw::spa::sys::SPA_PARAM_META_size,
                flags: pw::spa::pod::PropertyFlags::empty(),
                value: pw::spa::pod::Value::Int(std::mem::size_of::<MetaSyncTimeline>() as i32),
            },
        ],
    })
}

/// `SPA_META_Header` on each buffer: the producer's own stamp per frame (KWin's last
/// vblank up to 6.7, its paced tick from 6.8), which the wire pts takes over delivery time.
pub(super) fn build_header_meta_param() -> Result<Vec<u8>> {
    serialize_pod(pw::spa::pod::Object {
        type_: pw::spa::utils::SpaTypes::ObjectParamMeta.as_raw(),
        id: pw::spa::param::ParamType::Meta.as_raw(),
        properties: vec![
            pw::spa::pod::Property {
                key: pw::spa::sys::SPA_PARAM_META_type,
                flags: pw::spa::pod::PropertyFlags::empty(),
                value: pw::spa::pod::Value::Id(pw::spa::utils::Id(spa::sys::SPA_META_Header)),
            },
            pw::spa::pod::Property {
                key: pw::spa::sys::SPA_PARAM_META_size,
                flags: pw::spa::pod::PropertyFlags::empty(),
                value: pw::spa::pod::Value::Int(
                    std::mem::size_of::<spa::sys::spa_meta_header>() as i32
                ),
            },
        ],
    })
}

/// The producer's `maxFramerate` denominator, off one of its `EnumFormat` pods. KWin 6.8
/// offers millihertz (`refresh/1000`); 6.7 and older offer whole hertz. `None` without
/// the property.
pub(super) fn offer_framerate_denom(pod: &[u8]) -> Option<u32> {
    use pw::spa::pod::{deserialize::PodDeserializer, ChoiceValue, Value};
    use pw::spa::utils::{Choice, ChoiceEnum};
    let (_, Value::Object(obj)) = PodDeserializer::deserialize_any_from(pod).ok()? else {
        return None;
    };
    let prop = obj
        .properties
        .iter()
        .find(|p| p.key == pw::spa::sys::SPA_FORMAT_VIDEO_maxFramerate)?;
    match &prop.value {
        Value::Fraction(f) => Some(f.denom),
        Value::Choice(ChoiceValue::Fraction(Choice(_, ChoiceEnum::Range { default, .. })))
        | Value::Choice(ChoiceValue::Fraction(Choice(_, ChoiceEnum::Enum { default, .. }))) => {
            Some(default.denom)
        }
        _ => None,
    }
}

/// `SPA_META_Cursor` on each buffer, paired with the portal's
/// `CursorMode::Metadata`. Unsupported producers omit it (harmless).
pub(super) fn build_cursor_meta_param() -> Result<Vec<u8>> {
    fn meta_size(w: u32, h: u32) -> i32 {
        (std::mem::size_of::<spa::sys::spa_meta_cursor>()
            + std::mem::size_of::<spa::sys::spa_meta_bitmap>()
            + (w as usize * h as usize * 4)) as i32
    }
    serialize_pod(pw::spa::pod::Object {
        type_: pw::spa::utils::SpaTypes::ObjectParamMeta.as_raw(),
        id: pw::spa::param::ParamType::Meta.as_raw(),
        properties: vec![
            pw::spa::pod::Property {
                key: pw::spa::sys::SPA_PARAM_META_type,
                flags: pw::spa::pod::PropertyFlags::empty(),
                value: pw::spa::pod::Value::Id(pw::spa::utils::Id(spa::sys::SPA_META_Cursor)),
            },
            pw::spa::pod::Property {
                key: pw::spa::sys::SPA_PARAM_META_size,
                flags: pw::spa::pod::PropertyFlags::empty(),
                value: pw::spa::pod::Value::Choice(pw::spa::pod::ChoiceValue::Int(
                    pw::spa::utils::Choice(
                        pw::spa::utils::ChoiceFlags::empty(),
                        // `max` must cover the producer's offer or Meta fails
                        // silently and no buffer carries a cursor region.
                        // Mutter offers a fixed 384²; 1024² is headroom, not
                        // an allocation — the negotiated size is the producer's.
                        pw::spa::utils::ChoiceEnum::Range {
                            default: meta_size(64, 64),
                            min: meta_size(1, 1),
                            max: meta_size(1024, 1024),
                        },
                    ),
                )),
            },
        ],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `SPA_PARAM_BUFFERS_dataType` bitmask from a serialized Buffers pod.
    ///
    /// Literal SPA reader: a property is `{ key: u32, flags: u32, value: spa_pod }`
    /// and a pod is `{ size: u32, type: u32, body }`, so the `i32` sits 16 bytes
    /// past the key. The size word is itself `4`, which is why a heuristic
    /// "first plausible int" reads the wrong field.
    fn buffers_data_type(pod: &[u8]) -> i32 {
        let key = spa::sys::SPA_PARAM_BUFFERS_dataType.to_ne_bytes();
        let at = pod
            .windows(4)
            .position(|w| w == key)
            .expect("dataType key present in the Buffers pod");
        let word = |off: usize| u32::from_ne_bytes(pod[off..off + 4].try_into().unwrap());
        assert_eq!(word(at + 8), 4, "dataType's value pod should be 4 bytes");
        assert_eq!(
            word(at + 12),
            spa::sys::SPA_TYPE_Int,
            "dataType's value pod should be an Int"
        );
        i32::from_ne_bytes(pod[at + 16..at + 20].try_into().unwrap())
    }

    const MEM_PTR: i32 = 1 << spa::sys::SPA_DATA_MemPtr;
    const MEM_FD: i32 = 1 << spa::sys::SPA_DATA_MemFd;
    const DMABUF: i32 = 1 << spa::sys::SPA_DATA_DmaBuf;

    #[test]
    fn each_buffers_pod_requests_exactly_its_own_data_types() {
        assert_eq!(
            buffers_data_type(&build_mappable_buffers().unwrap()),
            MEM_PTR | MEM_FD | DMABUF,
            "the CPU path must accept mappable dmabufs too"
        );
        assert_eq!(
            buffers_data_type(&build_shm_only_buffers().unwrap()),
            MEM_PTR | MEM_FD,
            "PUNKTFUNK_FORCE_SHM must exclude DmaBuf"
        );
        assert_eq!(
            buffers_data_type(&build_dmabuf_buffers(crate::POOL_MIN, false).unwrap()),
            DMABUF,
            "the zero-copy/HDR path must exclude SHM"
        );
    }

    /// A malformed pod fails only at live negotiation, not at serialize.
    #[test]
    fn every_pod_round_trips_through_pod_from_bytes() {
        let mut pods: Vec<(&str, Vec<u8>)> = vec![
            ("mappable buffers", build_mappable_buffers().unwrap()),
            ("shm-only buffers", build_shm_only_buffers().unwrap()),
            (
                "dmabuf buffers",
                build_dmabuf_buffers(crate::POOL_MIN, false).unwrap(),
            ),
            (
                "kwin dmabuf buffers",
                build_dmabuf_buffers(crate::KWIN_POOL_MIN, false).unwrap(),
            ),
            (
                "explicit-sync dmabuf buffers",
                build_dmabuf_buffers(crate::KWIN_POOL_MIN, true).unwrap(),
            ),
            (
                "sync timeline meta",
                build_sync_timeline_meta_param().unwrap(),
            ),
            ("cursor meta", build_cursor_meta_param().unwrap()),
            (
                "default format",
                serialize_pod(build_default_format_obj(None, Pacing::Producer)).unwrap(),
            ),
            (
                "dmabuf BGRx",
                build_dmabuf_format(
                    VideoFormat::BGRx,
                    &[0, 1, 2],
                    Some((1920, 1080, 60)),
                    Pacing::Producer,
                )
                .unwrap(),
            ),
            (
                "dmabuf NV12",
                build_dmabuf_format(
                    VideoFormat::NV12,
                    &[0],
                    Some((1280, 720, 60)),
                    Pacing::Producer,
                )
                .unwrap(),
            ),
            (
                "hdr xRGB",
                build_hdr_dmabuf_format(VideoFormat::xRGB_210LE, &[0], None, Pacing::Producer)
                    .unwrap(),
            ),
            (
                "hdr xBGR",
                build_hdr_dmabuf_format(
                    VideoFormat::xBGR_210LE,
                    &[0],
                    Some((3840, 2160, 120)),
                    Pacing::Producer,
                )
                .unwrap(),
            ),
        ];
        for (name, bytes) in &mut pods {
            assert!(!bytes.is_empty(), "{name} serialized to nothing");
            assert_eq!(bytes.len() % 8, 0, "{name} is not 8-byte aligned/padded");
            assert!(
                spa::pod::Pod::from_bytes(bytes).is_some(),
                "{name} did not parse back as a pod"
            );
        }
    }

    /// KWin 6.8 is told apart from 6.7 by the denominator of the ceiling it offers.
    #[test]
    fn the_offer_denominator_tells_kwin_generations_apart() {
        use spa::pod::{ChoiceValue, Value};
        use spa::utils::{Choice, ChoiceEnum, ChoiceFlags, Fraction};
        let offer = |props: Vec<spa::pod::Property>| {
            serialize_pod(spa::pod::Object {
                type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
                id: spa::param::ParamType::EnumFormat.as_raw(),
                properties: props,
            })
            .unwrap()
        };
        let range = |num: u32, denom: u32| spa::pod::Property {
            key: spa::sys::SPA_FORMAT_VIDEO_maxFramerate,
            flags: spa::pod::PropertyFlags::empty(),
            value: Value::Choice(ChoiceValue::Fraction(Choice(
                ChoiceFlags::empty(),
                ChoiceEnum::Range {
                    default: Fraction { num, denom },
                    min: Fraction { num: 0, denom: 1 },
                    max: Fraction { num, denom },
                },
            ))),
        };
        assert_eq!(offer_framerate_denom(&offer(vec![range(120, 1)])), Some(1));
        assert_eq!(
            offer_framerate_denom(&offer(vec![range(120_000, 1_000)])),
            Some(1_000)
        );
        assert_eq!(offer_framerate_denom(&offer(vec![])), None);
        assert_eq!(offer_framerate_denom(b"not a pod"), None);
    }

    #[test]
    fn the_hdr_pods_carry_mandatory_pq_and_bt2020() {
        use spa::pod::{deserialize::PodDeserializer, ChoiceValue, Value};
        use spa::utils::{Choice, ChoiceEnum};

        for fmt in [
            VideoFormat::xRGB_210LE,
            VideoFormat::xBGR_210LE,
            VideoFormat::P010_10LE,
        ] {
            let pod = build_hdr_dmabuf_format(fmt, &[0], None, Pacing::Producer).unwrap();
            for (name, key) in [
                (
                    "transferFunction",
                    spa::sys::SPA_FORMAT_VIDEO_transferFunction,
                ),
                ("colorPrimaries", spa::sys::SPA_FORMAT_VIDEO_colorPrimaries),
                ("modifier", spa::sys::SPA_FORMAT_VIDEO_modifier),
            ] {
                assert!(
                    pod.windows(4).any(|w| w == key.to_ne_bytes()),
                    "{fmt:?} pod is missing {name}"
                );
            }
            assert!(
                pod.windows(4)
                    .any(|w| w == SPA_VIDEO_TRANSFER_SMPTE2084.to_ne_bytes()),
                "{fmt:?} pod does not carry the PQ transfer id"
            );
            assert!(
                pod.windows(4)
                    .any(|w| w == spa::sys::SPA_VIDEO_COLOR_PRIMARIES_BT2020.to_ne_bytes()),
                "{fmt:?} pod does not carry BT.2020 primaries"
            );
            let (_, Value::Object(obj)) =
                PodDeserializer::deserialize_any_from(&pod).expect("parse HDR format pod")
            else {
                panic!("{fmt:?} HDR format is not an object pod");
            };
            let modifier = obj
                .properties
                .iter()
                .find(|p| p.key == spa::sys::SPA_FORMAT_VIDEO_modifier)
                .expect("HDR modifier property");
            let Value::Choice(ChoiceValue::Long(Choice(
                _,
                ChoiceEnum::Enum {
                    default,
                    alternatives,
                },
            ))) = &modifier.value
            else {
                panic!("{fmt:?} HDR modifier is not a Long choice enum");
            };
            assert_eq!((*default, alternatives.as_slice()), (0, &[0][..]));
        }
    }

    /// The YUV offers pin the matrix the bitstream declares; packed RGB never does.
    #[test]
    fn only_the_planar_offers_pin_the_colour_matrix() {
        let nv12 = build_dmabuf_format(VideoFormat::NV12, &[0], None, Pacing::Producer).unwrap();
        let bgrx = build_dmabuf_format(VideoFormat::BGRx, &[0], None, Pacing::Producer).unwrap();
        let p010 =
            build_hdr_dmabuf_format(VideoFormat::P010_10LE, &[0], None, Pacing::Producer).unwrap();
        let xbgr =
            build_hdr_dmabuf_format(VideoFormat::xBGR_210LE, &[0], None, Pacing::Producer).unwrap();
        for (name, key) in [
            ("colorMatrix", spa::sys::SPA_FORMAT_VIDEO_colorMatrix),
            ("colorRange", spa::sys::SPA_FORMAT_VIDEO_colorRange),
        ] {
            assert!(
                nv12.windows(4).any(|w| w == key.to_ne_bytes()),
                "NV12 offer is missing {name}"
            );
            assert!(
                p010.windows(4).any(|w| w == key.to_ne_bytes()),
                "P010 offer is missing {name}"
            );
            assert!(
                !bgrx.windows(4).any(|w| w == key.to_ne_bytes()),
                "packed-RGB offer should not pin {name}"
            );
            assert!(
                !xbgr.windows(4).any(|w| w == key.to_ne_bytes()),
                "packed 10-bit offer should not pin {name}"
            );
        }
        assert!(
            p010.windows(4)
                .any(|w| w == spa::sys::SPA_VIDEO_COLOR_MATRIX_BT2020.to_ne_bytes()),
            "P010 pins BT.2020"
        );
    }

    /// Hand-written PQ id vs the real libspa binding, wherever the symbol
    /// exists. A renumbered enum would silently tag HDR with the wrong transfer.
    #[test]
    fn pq_transfer_id_matches_libspa() {
        assert_eq!(
            super::SPA_VIDEO_TRANSFER_SMPTE2084,
            super::pw::spa::sys::SPA_VIDEO_TRANSFER_SMPTE2084,
            "libspa renumbered spa_video_transfer_function — update the hardcoded PQ id"
        );
    }

    /// Pool depth must be a Choice Range. A fixed Int a producer cannot afford
    /// `unpaced` must put `maxFramerate = 0/1` on the wire: KWin reads a non-zero
    /// ceiling as "schedule on a timer" and rounds each wait up to a whole
    /// millisecond. A ceiling that survives here is the jitter we came to remove.
    #[test]
    fn a_capped_offer_carries_the_wire_rate() {
        let producer =
            build_dmabuf_format(VideoFormat::BGRx, &[0], None, Pacing::Producer).unwrap();
        let capped = build_dmabuf_format(VideoFormat::BGRx, &[0], None, Pacing::Cap(90)).unwrap();
        let unpaced = build_dmabuf_format(VideoFormat::BGRx, &[0], None, Pacing::Unpaced).unwrap();
        assert!(
            capped.len() > producer.len(),
            "the cap adds the maxFramerate property"
        );
        assert_eq!(capped.len(), unpaced.len(), "same property, another value");
        assert_ne!(capped, unpaced, "90/1 is not 0/1");
    }

    #[test]
    fn the_unpaced_offer_carries_a_zero_max_framerate() {
        let paced = build_dmabuf_format(VideoFormat::BGRx, &[0], None, Pacing::Producer).unwrap();
        let unpaced = build_dmabuf_format(VideoFormat::BGRx, &[0], None, Pacing::Unpaced).unwrap();
        // Property = { key, flags, value_pod }; value_pod = { size, type, body }.
        // 4 + 4 + 4 + 4 + a two-word Fraction body = 24.
        assert_eq!(
            unpaced.len(),
            paced.len() + 24,
            "the unpaced offer must add exactly the maxFramerate property"
        );
        let key = spa::sys::SPA_FORMAT_VIDEO_maxFramerate.to_ne_bytes();
        let at = unpaced
            .windows(4)
            .position(|w| w == key)
            .expect("the unpaced offer must carry a maxFramerate");
        let word = |off: usize| u32::from_ne_bytes(unpaced[off..off + 4].try_into().unwrap());
        assert_eq!(word(at + 8), 8, "a Fraction value pod is 8 bytes");
        assert_eq!(
            word(at + 12),
            spa::sys::SPA_TYPE_Fraction,
            "…of type Fraction"
        );
        assert_eq!(
            (word(at + 16), word(at + 20)),
            (0, 1),
            "0/1 is what zeroes KWin's frameInterval(); any other value re-arms the timer"
        );
    }

    /// empties the Buffers intersection and stalls the link with no error.
    #[test]
    fn the_dmabuf_pool_request_is_a_range_not_a_fixed_count() {
        for pool_min in [crate::POOL_MIN, crate::KWIN_POOL_MIN] {
            let pod = build_dmabuf_buffers(pool_min, false).unwrap();
            let key = spa::sys::SPA_PARAM_BUFFERS_buffers.to_ne_bytes();
            let at = pod
                .windows(4)
                .position(|w| w == key)
                .expect("the dmabuf Buffers pod must carry a buffers count");
            let word = |off: usize| u32::from_ne_bytes(pod[off..off + 4].try_into().unwrap());
            // Property = { key, flags, value_pod }; value_pod = { size, type, body }.
            // Choice body = { type, flags, child_size, child_type, values… }.
            assert_eq!(
                word(at + 12),
                spa::sys::SPA_TYPE_Choice,
                "the buffers count must be a Choice, not a bare Int — a fixed count can fail \
                 negotiation outright"
            );
            assert_eq!(
                word(at + 16),
                spa::sys::SPA_CHOICE_Range,
                "the Choice must be a Range (default, min, max)"
            );
            assert_eq!(word(at + 24), 4, "Choice child pods are 4-byte Ints");
            assert_eq!(word(at + 28), spa::sys::SPA_TYPE_Int, "…of type Int");
            let vals: Vec<i32> = (0..3)
                .map(|i| {
                    i32::from_ne_bytes(pod[at + 32 + i * 4..at + 36 + i * 4].try_into().unwrap())
                })
                .collect();
            assert_eq!(
                vals,
                vec![POOL_DEFAULT, pool_min, POOL_MAX],
                "Range values are serialized default-first"
            );
        }
        // The minimum must not exceed what producers already serve, or the ask becomes a demand.
        const { assert!(crate::POOL_MIN <= 2) };
        // KWin ≥ 6.2 caps its pool at 4; a higher minimum fails negotiation outright.
        const { assert!(crate::KWIN_POOL_MAX == 4 && crate::KWIN_POOL_MIN <= crate::KWIN_POOL_MAX) };
    }

    /// Without MANDATORY a producer lacking the meta would still match, and this side would
    /// then wait on sync datas that never arrive.
    #[test]
    fn the_explicit_sync_pod_demands_the_sync_meta() {
        let key = spa::sys::SPA_PARAM_BUFFERS_metaType.to_ne_bytes();
        let plain = build_dmabuf_buffers(crate::POOL_MIN, false).unwrap();
        assert!(
            !plain.windows(4).any(|w| w == key),
            "the plain twin must leave metaType to the producer"
        );
        let pod = build_dmabuf_buffers(crate::POOL_MIN, true).unwrap();
        let at = pod
            .windows(4)
            .position(|w| w == key)
            .expect("the explicit-sync pod must carry metaType");
        let word = |off: usize| u32::from_ne_bytes(pod[off..off + 4].try_into().unwrap());
        // Property = { key, flags, value_pod }; value_pod = { size, type, body }.
        assert_eq!(word(at + 4), spa::sys::SPA_POD_PROP_FLAG_MANDATORY);
        assert_eq!(word(at + 12), spa::sys::SPA_TYPE_Int);
        assert_eq!(word(at + 16), 1 << spa::sys::SPA_META_SyncTimeline);
    }

    #[test]
    fn hdr_offers_xbgr_before_xrgb() {
        assert_eq!(
            HDR_FORMAT_ORDER[0],
            VideoFormat::xBGR_210LE,
            "xBGR_210LE must be offered first — leading with xRGB_210LE swaps red and blue on \
             every NVIDIA gamescope HDR session"
        );
        assert_eq!(
            HDR_FORMAT_ORDER[1],
            VideoFormat::xRGB_210LE,
            "xRGB_210LE stays as the fallback pod so a producer offering only it can still \
             negotiate HDR instead of dropping to the SDR downgrade"
        );
        // Both must still build: the order is a preference, never a removal.
        for fmt in HDR_FORMAT_ORDER {
            assert!(
                !build_hdr_dmabuf_format(fmt, &[0], None, Pacing::Producer)
                    .unwrap()
                    .is_empty(),
                "{fmt:?} must still produce a format pod"
            );
        }
    }
}
