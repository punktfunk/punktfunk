//! The PyroWave open as a plan: what an open asks the GPU for, decided from what the shell read
//! off the picked device before it creates anything.
//!
//! Pure: no `ash`, no `pyrowave_sys`, so the table below runs on every target. The shell is
//! `open_inner` in `pyrowave.rs`: it creates the instance, picks the device, samples
//! [`DeviceFacts`], then runs each [`OpenStep`] of [`plan_open`] in order. A failed step drops
//! the half-built encoder, whose `Drop` is the unwind.
//!
//! Invariants: a GPU without every feature the shaders use is refused before a device exists; a
//! priority ladder rides only on a device that enables a global-priority extension; only
//! `PUNKTFUNK_PERF` adds the calibrated clock; the planes and the CSC shader follow the
//! negotiated chroma, depth and colour. Each row of `open_table` and each ladder test names the
//! rule it pins. Design: `design/pf-encode-overhaul.md` §4.4.

use anyhow::{bail, Result};
use std::ffi::{CStr, CString};

/// Independent per-frame resource sets. Two: Granite's device defaults to
/// `init_frame_contexts(2)` and `next_frame_context()` waits the context it rotates
/// into, so frame N may not begin until N-2 completed. A third slot needs a
/// vendored `init_frame_contexts(3)`, which is not exposed. `max_inflight` is how
/// many are live; this is capacity only.
pub(crate) const SLOTS: usize = 2;

/// PCI vendor id of NVIDIA, whose REALTIME queues pay extra on every submit.
pub(crate) const VENDOR_NVIDIA: u32 = 0x10de;

pub(crate) const EXT_MEMORY_FD: &CStr = c"VK_KHR_external_memory_fd";
pub(crate) const EXT_DMA_BUF: &CStr = c"VK_EXT_external_memory_dma_buf";
pub(crate) const EXT_DRM_MODIFIER: &CStr = c"VK_EXT_image_drm_format_modifier";
pub(crate) const EXT_FOREIGN: &CStr = c"VK_EXT_queue_family_foreign";
pub(crate) const EXT_PRIORITY_KHR: &CStr = c"VK_KHR_global_priority";
pub(crate) const EXT_PRIORITY_EXT: &CStr = c"VK_EXT_global_priority";
pub(crate) const EXT_CALIBRATED_KHR: &CStr = c"VK_KHR_calibrated_timestamps";
pub(crate) const EXT_CALIBRATED_EXT: &CStr = c"VK_EXT_calibrated_timestamps";
/// The extensions the plan enables only where the device advertises them.
pub(crate) const PROBED: [&CStr; 5] = [
    EXT_FOREIGN,
    EXT_PRIORITY_KHR,
    EXT_PRIORITY_EXT,
    EXT_CALIBRATED_KHR,
    EXT_CALIBRATED_EXT,
];

/// The Vulkan 1.0–1.3 features the open reads off the device and enables on it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Features {
    pub shader_int16: bool,
    pub storage_buffer16: bool,
    pub storage_buffer8: bool,
    pub timeline_semaphore: bool,
    pub subgroup_size_control: bool,
    pub compute_full_subgroups: bool,
    pub synchronization2: bool,
    pub shader_float16: bool,
    pub memory_model: bool,
    pub memory_model_device_scope: bool,
    pub maintenance4: bool,
}

impl Features {
    /// What pyrowave's encoder checks (shaderInt16, storageBuffer8BitAccess, subgroupSizeControl,
    /// computeFullSubgroups), plus storageBuffer16BitAccess for its shaders' 16-bit buffer
    /// members, synchronization2 for this host's barriers, and timelineSemaphore. The other four
    /// are optional.
    fn required(&self) -> [(bool, &'static str); 7] {
        [
            (self.shader_int16, "shaderInt16"),
            (self.storage_buffer16, "storageBuffer16BitAccess"),
            (self.storage_buffer8, "storageBuffer8BitAccess"),
            (self.timeline_semaphore, "timelineSemaphore"),
            (self.subgroup_size_control, "subgroupSizeControl"),
            (self.compute_full_subgroups, "computeFullSubgroups"),
            (self.synchronization2, "synchronization2"),
        ]
    }
}

/// What the shell read off the picked device, and the session it opens.
#[derive(Clone, Debug, Default)]
pub(crate) struct DeviceFacts<'a> {
    pub vendor_id: u32,
    /// The device's first graphics+compute queue family.
    pub family: u32,
    /// The extensions of [`PROBED`] it advertises.
    pub extensions: Vec<CString>,
    pub features: Features,
    /// `PUNKTFUNK_PERF`: split each encode's time on the GPU.
    pub perf: bool,
    /// `PYROWAVE_QUEUE_PRIORITY` as given; `None` asks for the default class.
    pub intent: Option<&'a str>,
    pub width: u32,
    pub height: u32,
    pub chroma444: bool,
    /// The negotiated stream depth, 8 or 10.
    pub bit_depth: u8,
    /// The session asked for BT.2020 PQ.
    pub hdr: bool,
}

impl DeviceFacts<'_> {
    /// The planes hold `code10 << 6`.
    pub(crate) fn ten_bit(&self) -> bool {
        self.bit_depth >= 10
    }

    /// PQ labelling needs the 10-bit stream: an 8-bit HDR ask encodes BT.709 codes.
    pub(crate) fn pq(&self) -> bool {
        self.hdr && self.ten_bit()
    }
}

/// A global queue priority class.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Priority {
    Realtime,
    High,
}

/// The CSC shader, `rgb2yuv*.comp`, by chroma, depth and colour.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Shader {
    /// `rgb2yuv`: BT.709 full range, 8-bit.
    Sdr420,
    /// `rgb2yuv444`.
    Sdr444,
    /// `rgb2yuv10_709`: BT.709, the 8-bit capture widened to 10-bit codes.
    Sdr420x10,
    /// `rgb2yuv444_10_709`.
    Sdr444x10,
    /// `rgb2yuv10`: BT.2020 NCL over PQ-coded input.
    Pq420,
    /// `rgb2yuv444_10`.
    Pq444,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PlaneFormat {
    R8,
    Rg8,
    R16,
    Rg16,
}

/// An image the CSC writes and the wavelet samples.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Plane {
    pub format: PlaneFormat,
    pub width: u32,
    pub height: u32,
}

/// The device an open creates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DeviceStep {
    /// Enabled in this order.
    pub extensions: Vec<&'static CStr>,
    pub features: Features,
    /// The family of its one queue.
    pub family: u32,
    /// The extension the ladder rides on; `None` exactly when the ladder is empty.
    pub priority_ext: Option<&'static CStr>,
    /// Classes to try in order, stepping down on a refusal. Every class refused, or none
    /// asked: the device is created without a priority request.
    pub ladder: Vec<Priority>,
}

/// One stage of an open, with what it asks the device for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum OpenStep {
    Device(DeviceStep),
    /// Queue 0 of `family`. Fresh dmabufs are acquired from FOREIGN when `foreign`, else from
    /// QUEUE_FAMILY_EXTERNAL.
    Queue {
        family: u32,
        foreign: bool,
    },
    /// The pyrowave device on that queue, and one encoder per slot at this size and chroma.
    PyroWave {
        encoders: usize,
        width: u32,
        height: u32,
        chroma444: bool,
    },
    /// The CSC pipeline running `shader`, and a pool of `sets` descriptor sets. Each set samples
    /// the frame and the slot's cursor plane, and stores luma and chroma.
    Csc {
        shader: Shader,
        sets: u32,
        sampled: u32,
        storage: u32,
    },
    /// The command pool, then `count` slots: each the two planes, a cursor plane, a descriptor
    /// set, a command buffer and a fence.
    Slots {
        count: usize,
        luma: Plane,
        chroma: Plane,
    },
    /// `PUNKTFUNK_PERF`'s timestamp queries, calibrated through `calibrate` when there is one.
    Timer {
        calibrate: Option<&'static CStr>,
    },
}

/// Global-priority classes for `PYROWAVE_QUEUE_PRIORITY`, the grammar of
/// `patches/0005-global-priority-queue.patch`: ASCII-lowercased; `off` → none;
/// `high` → `[HIGH]`; `realtime`, unset and junk → `[REALTIME, HIGH]`.
/// Linux adds one rule: unset or junk on an NVIDIA device → `[HIGH]`. Its REALTIME
/// queue makes every `vkQueueSubmit` cost 0.4–1 ms, more than the preemption saves.
/// The same variable drives Windows (the patch) and Linux (our create-infos, which
/// Granite takes as `inherit_info`). `off` is the only disable spelling; `0` is not.
/// Do not change the grammar without the patch in the same commit.
fn queue_priority_candidates(raw: Option<&str>, vendor_id: u32) -> Vec<Priority> {
    let want = raw.map(|s| s.to_ascii_lowercase());
    let ladder = vec![Priority::Realtime, Priority::High];
    match want.as_deref() {
        Some("off") => Vec::new(),
        Some("high") => vec![Priority::High],
        Some("realtime") => ladder,
        _ if vendor_id == VENDOR_NVIDIA => vec![Priority::High],
        _ => ladder,
    }
}

/// The steps an open runs, in order, or why this GPU cannot encode. The feature check follows
/// the pick rather than steering it: a pick that skips this GPU can land on one that cannot
/// import the capturer's buffers and trip that capture's raw-dmabuf latch. The refusal is
/// latch-free; the session layer renegotiates.
pub(crate) fn plan_open(f: &DeviceFacts) -> Result<Vec<OpenStep>> {
    let missing: Vec<&str> = f
        .features
        .required()
        .into_iter()
        .filter(|(ok, _)| !ok)
        .map(|(_, name)| name)
        .collect();
    if !missing.is_empty() {
        bail!("GPU lacks pyrowave-required Vulkan features: {missing:?}");
    }
    let has = |name: &CStr| f.extensions.iter().any(|e| e.as_c_str() == name);
    let foreign = has(EXT_FOREIGN);
    let ladder = queue_priority_candidates(f.intent, f.vendor_id);
    // KHR is the promoted name; pf-zerocopy's VkBridge probes in the same order.
    let priority_ext = [EXT_PRIORITY_KHR, EXT_PRIORITY_EXT]
        .into_iter()
        .find(|&n| has(n))
        .filter(|_| !ladder.is_empty());
    let calibrate = [EXT_CALIBRATED_KHR, EXT_CALIBRATED_EXT]
        .into_iter()
        .find(|&n| f.perf && has(n));
    let extensions = [EXT_MEMORY_FD, EXT_DMA_BUF, EXT_DRM_MODIFIER]
        .into_iter()
        .chain(foreign.then_some(EXT_FOREIGN))
        .chain(priority_ext)
        .chain(calibrate)
        .collect();
    let (w, h, chroma444) = (f.width, f.height, f.chroma444);
    let (cw, ch) = if chroma444 { (w, h) } else { (w / 2, h / 2) };
    // The wavelet reads UNORM samples, so depth is a plane format, not a codec flag.
    let (luma, chroma) = if f.ten_bit() {
        (PlaneFormat::R16, PlaneFormat::Rg16)
    } else {
        (PlaneFormat::R8, PlaneFormat::Rg8)
    };
    let shader = match (chroma444, f.ten_bit(), f.pq()) {
        (false, false, _) => Shader::Sdr420,
        (true, false, _) => Shader::Sdr444,
        (false, true, false) => Shader::Sdr420x10,
        (false, true, true) => Shader::Pq420,
        (true, true, false) => Shader::Sdr444x10,
        (true, true, true) => Shader::Pq444,
    };
    let mut steps = vec![
        OpenStep::Device(DeviceStep {
            extensions,
            // Every required feature is there (checked above); the optional ones follow the GPU.
            features: f.features,
            family: f.family,
            priority_ext,
            ladder: if priority_ext.is_some() {
                ladder
            } else {
                Vec::new()
            },
        }),
        OpenStep::Queue {
            family: f.family,
            foreign,
        },
        OpenStep::PyroWave {
            encoders: SLOTS,
            width: w,
            height: h,
            chroma444,
        },
        OpenStep::Csc {
            shader,
            sets: SLOTS as u32,
            sampled: 2 * SLOTS as u32,
            storage: 2 * SLOTS as u32,
        },
        OpenStep::Slots {
            count: SLOTS,
            luma: Plane {
                format: luma,
                width: w,
                height: h,
            },
            chroma: Plane {
                format: chroma,
                width: cw,
                height: ch,
            },
        },
    ];
    if f.perf {
        steps.push(OpenStep::Timer { calibrate });
    }
    Ok(steps)
}

#[cfg(test)]
mod tests {
    use super::OpenStep::*;
    use super::PlaneFormat::*;
    use super::*;

    const AMD: u32 = 0x1002;
    const LADDER: [Priority; 2] = [Priority::Realtime, Priority::High];
    const ALL: Features = Features {
        shader_int16: true,
        storage_buffer16: true,
        storage_buffer8: true,
        timeline_semaphore: true,
        subgroup_size_control: true,
        compute_full_subgroups: true,
        synchronization2: true,
        shader_float16: true,
        memory_model: true,
        memory_model_device_scope: true,
        maintenance4: true,
    };

    /// AMD, 1920×1080 4:2:0 8-bit SDR on family 1, every feature and every extension the open
    /// looks for, no `PUNKTFUNK_PERF`; then `edit`.
    fn facts(edit: impl FnOnce(&mut DeviceFacts<'static>)) -> DeviceFacts<'static> {
        let mut f = DeviceFacts {
            vendor_id: AMD,
            family: 1,
            extensions: [
                c"VK_KHR_swapchain",
                EXT_FOREIGN,
                EXT_PRIORITY_EXT,
                EXT_PRIORITY_KHR,
                EXT_CALIBRATED_EXT,
                EXT_CALIBRATED_KHR,
            ]
            .map(CStr::to_owned)
            .to_vec(),
            features: ALL,
            perf: false,
            intent: None,
            width: 1920,
            height: 1080,
            chroma444: false,
            bit_depth: 8,
            hdr: false,
        };
        edit(&mut f);
        f
    }

    fn drop_ext(f: &mut DeviceFacts, names: &[&CStr]) {
        f.extensions.retain(|e| !names.contains(&e.as_c_str()));
    }

    /// The device step of `facts(|_| {})`; then `edit`.
    fn device(edit: impl FnOnce(&mut DeviceStep)) -> OpenStep {
        let mut d = DeviceStep {
            extensions: vec![
                EXT_MEMORY_FD,
                EXT_DMA_BUF,
                EXT_DRM_MODIFIER,
                EXT_FOREIGN,
                EXT_PRIORITY_KHR,
            ],
            features: ALL,
            family: 1,
            priority_ext: Some(EXT_PRIORITY_KHR),
            ladder: LADDER.to_vec(),
        };
        edit(&mut d);
        Device(d)
    }

    fn plane(format: PlaneFormat, width: u32, height: u32) -> Plane {
        Plane {
            format,
            width,
            height,
        }
    }

    fn csc(shader: Shader) -> OpenStep {
        Csc {
            shader,
            sets: 2,
            sampled: 4,
            storage: 4,
        }
    }

    fn slots(luma: Plane, chroma: Plane) -> OpenStep {
        Slots {
            count: 2,
            luma,
            chroma,
        }
    }

    fn full_chroma() -> OpenStep {
        PyroWave {
            encoders: 2,
            width: 1920,
            height: 1080,
            chroma444: true,
        }
    }

    /// The plan of `facts(|_| {})`, each of `edits` in place of the step of its kind, or
    /// appended when the plan has none.
    fn plan(edits: Vec<OpenStep>) -> Vec<OpenStep> {
        let mut p = vec![
            device(|_| {}),
            Queue {
                family: 1,
                foreign: true,
            },
            PyroWave {
                encoders: 2,
                width: 1920,
                height: 1080,
                chroma444: false,
            },
            csc(Shader::Sdr420),
            slots(plane(R8, 1920, 1080), plane(Rg8, 960, 540)),
        ];
        for e in edits {
            let kind = std::mem::discriminant(&e);
            match p.iter_mut().find(|s| std::mem::discriminant(&**s) == kind) {
                Some(s) => *s = e,
                None => p.push(e),
            }
        }
        p
    }

    /// A row: the rule it pins, the facts, the plan or the refusal.
    type Row = (
        &'static str,
        DeviceFacts<'static>,
        Result<Vec<OpenStep>, &'static str>,
    );

    #[test]
    fn open_table() {
        let rows: Vec<Row> = vec![
            (
                "two slots, each sampling its own cursor plane beside the frame",
                facts(|_| {}),
                Ok(plan(vec![])),
            ),
            (
                "enable 16-bit storage buffers on the encode device",
                facts(|f| f.features.storage_buffer16 = false),
                Err(r#"GPU lacks pyrowave-required Vulkan features: ["storageBuffer16BitAccess"]"#),
            ),
            (
                "name every missing feature in pyrowave.h's order",
                facts(|f| f.features = Features::default()),
                Err(concat!(
                    r#"GPU lacks pyrowave-required Vulkan features: ["shaderInt16", "#,
                    r#""storageBuffer16BitAccess", "storageBuffer8BitAccess", "#,
                    r#""timelineSemaphore", "subgroupSizeControl", "computeFullSubgroups", "#,
                    r#""synchronization2"]"#
                )),
            ),
            (
                "enable the optional features only where the GPU has them",
                facts(|f| {
                    f.features.shader_float16 = false;
                    f.features.memory_model = false;
                    f.features.memory_model_device_scope = false;
                    f.features.maintenance4 = false;
                }),
                Ok(plan(vec![device(|d| {
                    d.features.shader_float16 = false;
                    d.features.memory_model = false;
                    d.features.memory_model_device_scope = false;
                    d.features.maintenance4 = false;
                })])),
            ),
            (
                "acquire from QUEUE_FAMILY_EXTERNAL without the foreign extension",
                facts(|f| drop_ext(f, &[EXT_FOREIGN])),
                Ok(plan(vec![
                    device(|d| d.extensions.retain(|&e| e != EXT_FOREIGN)),
                    Queue {
                        family: 1,
                        foreign: false,
                    },
                ])),
            ),
            (
                "fall back to the EXT global-priority name",
                facts(|f| drop_ext(f, &[EXT_PRIORITY_KHR])),
                Ok(plan(vec![device(|d| {
                    d.extensions[4] = EXT_PRIORITY_EXT;
                    d.priority_ext = Some(EXT_PRIORITY_EXT);
                })])),
            ),
            (
                "ask no priority of a device without the extension",
                facts(|f| drop_ext(f, &[EXT_PRIORITY_KHR, EXT_PRIORITY_EXT])),
                Ok(plan(vec![device(|d| {
                    d.extensions.pop();
                    d.priority_ext = None;
                    d.ladder.clear();
                })])),
            ),
            (
                "add the calibrated clock and the timer under PUNKTFUNK_PERF",
                facts(|f| f.perf = true),
                Ok(plan(vec![
                    device(|d| d.extensions.push(EXT_CALIBRATED_KHR)),
                    Timer {
                        calibrate: Some(EXT_CALIBRATED_KHR),
                    },
                ])),
            ),
            (
                "fall back to the EXT calibrated-timestamps name",
                facts(|f| {
                    f.perf = true;
                    drop_ext(f, &[EXT_CALIBRATED_KHR]);
                }),
                Ok(plan(vec![
                    device(|d| d.extensions.push(EXT_CALIBRATED_EXT)),
                    Timer {
                        calibrate: Some(EXT_CALIBRATED_EXT),
                    },
                ])),
            ),
            (
                "time uncalibrated where no calibrated clock is advertised",
                facts(|f| {
                    f.perf = true;
                    drop_ext(f, &[EXT_CALIBRATED_KHR, EXT_CALIBRATED_EXT]);
                }),
                Ok(plan(vec![Timer { calibrate: None }])),
            ),
            (
                "4:4:4 keeps chroma at full resolution",
                facts(|f| f.chroma444 = true),
                Ok(plan(vec![
                    full_chroma(),
                    csc(Shader::Sdr444),
                    slots(plane(R8, 1920, 1080), plane(Rg8, 1920, 1080)),
                ])),
            ),
            (
                "10-bit SDR widens to R16 planes under BT.709",
                facts(|f| f.bit_depth = 10),
                Ok(plan(vec![
                    csc(Shader::Sdr420x10),
                    slots(plane(R16, 1920, 1080), plane(Rg16, 960, 540)),
                ])),
            ),
            (
                "HDR is BT.2020 PQ on the 10-bit stream",
                facts(|f| {
                    f.bit_depth = 10;
                    f.hdr = true;
                }),
                Ok(plan(vec![
                    csc(Shader::Pq420),
                    slots(plane(R16, 1920, 1080), plane(Rg16, 960, 540)),
                ])),
            ),
            (
                "an 8-bit HDR ask encodes BT.709 codes",
                facts(|f| f.hdr = true),
                Ok(plan(vec![])),
            ),
            (
                "4:4:4 10-bit SDR",
                facts(|f| {
                    f.chroma444 = true;
                    f.bit_depth = 10;
                }),
                Ok(plan(vec![
                    full_chroma(),
                    csc(Shader::Sdr444x10),
                    slots(plane(R16, 1920, 1080), plane(Rg16, 1920, 1080)),
                ])),
            ),
            (
                "4:4:4 HDR",
                facts(|f| {
                    f.chroma444 = true;
                    f.bit_depth = 10;
                    f.hdr = true;
                }),
                Ok(plan(vec![
                    full_chroma(),
                    csc(Shader::Pq444),
                    slots(plane(R16, 1920, 1080), plane(Rg16, 1920, 1080)),
                ])),
            ),
        ];
        for (name, f, want) in rows {
            let got = plan_open(&f).map_err(|e| e.to_string());
            assert_eq!(got, want.map_err(String::from), "{name}");
        }
    }

    // The ladder reaches the device only through `plan_open`. These pin the grammar
    // `PYROWAVE_QUEUE_PRIORITY` shares with the Windows patch; the walk itself needs a GPU.

    /// The device step for `intent` on `vendor_id`.
    fn device_for(intent: Option<&'static str>, vendor_id: u32) -> DeviceStep {
        let f = facts(|f| {
            f.intent = intent;
            f.vendor_id = vendor_id;
        });
        match plan_open(&f).unwrap().remove(0) {
            Device(d) => d,
            s => panic!("the plan opens with {s:?}"),
        }
    }

    fn ladder(intent: Option<&'static str>, vendor_id: u32) -> Vec<Priority> {
        device_for(intent, vendor_id).ladder
    }

    /// Unset means the realtime ladder (REALTIME then HIGH), not a single class.
    #[test]
    fn unset_requests_the_realtime_ladder() {
        assert_eq!(ladder(None, AMD), LADDER);
    }

    /// Unset on NVIDIA means HIGH alone: its REALTIME queue slows every submit.
    #[test]
    fn unset_on_nvidia_requests_high() {
        assert_eq!(ladder(None, VENDOR_NVIDIA), [Priority::High]);
        assert_eq!(ladder(Some("junk"), VENDOR_NVIDIA), [Priority::High]);
    }

    /// An explicit `realtime` wins over the NVIDIA default.
    #[test]
    fn realtime_asks_for_the_ladder_on_every_vendor() {
        for vendor in [AMD, VENDOR_NVIDIA] {
            assert_eq!(ladder(Some("REALTIME"), vendor), LADDER);
        }
    }

    /// `off` is the only disable spelling (case-insensitive), and it enables no priority
    /// extension. `0` is not: the C side does not accept it either.
    #[test]
    fn only_off_disables_and_it_is_case_insensitive() {
        for vendor in [AMD, VENDOR_NVIDIA] {
            for off in ["off", "OFF", "Off"] {
                let d = device_for(Some(off), vendor);
                assert!(d.ladder.is_empty(), "{off}");
                assert_eq!(d.priority_ext, None, "{off}");
                assert!(!d.extensions.contains(&EXT_PRIORITY_KHR), "{off}");
            }
            assert!(!ladder(Some("0"), vendor).is_empty());
        }
    }

    /// `high` is HIGH only — silently trying REALTIME first would hide "elevated, not realtime".
    #[test]
    fn high_asks_for_high_alone() {
        assert_eq!(ladder(Some("high"), AMD), [Priority::High]);
        assert_eq!(ladder(Some("HIGH"), AMD), [Priority::High]);
    }

    /// Junk falls back to the default ladder, not to off: unparseable must not disable the lever.
    #[test]
    fn junk_falls_back_to_the_default_ladder() {
        for raw in ["", "yes", "1", "medium", "  high"] {
            assert_eq!(ladder(Some(raw), AMD), LADDER, "{raw:?}");
        }
    }
}
