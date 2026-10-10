//! `VADRMPRIMESurfaceDescriptor` from `vaExportSurfaceHandle`, and the walk that
//! turns it into the plane list a dmabuf import consumes.
//!
//! The DRIVER writes this structure; every other buffer in the crate is one we
//! fill. Layout is measured by `layout-probe.c`. A wrong fd or plane count still
//! imports, so [`flatten`] bounds-checks before walking.
//!
//! Under `VA_EXPORT_SURFACE_SEPARATE_LAYERS` an NV12 surface is two layers
//! (`R8` luma, `GR88` chroma), one plane each — not one two-plane `NV12` layer.
//! [`flatten`] walks every plane of every layer in declared order and takes
//! fourcc from the descriptor's top-level field, never a layer's component
//! format. `VA_EXPORT_SURFACE_COMPOSED_LAYERS` is not universally implemented.

use std::os::raw::c_int;

/// Decoder may still write later surfaces; the consumer only samples.
pub const VA_EXPORT_SURFACE_READ_ONLY: u32 = 0x0001;

/// One layer per plane, not one composed NV12 layer.
pub const VA_EXPORT_SURFACE_SEPARATE_LAYERS: u32 = 0x0004;

/// Packed `DRM_FORMAT_NV12`; a DRM importer takes it unchanged.
pub const VA_FOURCC_NV12: u32 = 0x3231_564e;

/// Packed `DRM_FORMAT_P010`; a DRM importer takes it unchanged.
pub const VA_FOURCC_P010: u32 = 0x3031_3050;

/// Descriptor array bounds, measured (`layout-probe.c`).
pub const MAX_OBJECTS: usize = 4;
pub const MAX_LAYERS: usize = 4;
pub const MAX_PLANES_PER_LAYER: usize = 4;

/// One BO. The fd is owned; the caller must close it.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct VaDrmPrimeObject {
    /// PRIME fd as `c_int`. Wrap in `OwnedFd` on successful export.
    pub fd: c_int,
    pub size: u32,
    pub drm_format_modifier: u64,
}

/// One layer: a single plane and its component format under `SEPARATE_LAYERS`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct VaDrmPrimeLayer {
    /// Component format (`R8`, `GR88`), never the surface fourcc.
    pub drm_format: u32,
    pub num_planes: u32,
    pub object_index: [u32; MAX_PLANES_PER_LAYER],
    pub offset: [u32; MAX_PLANES_PER_LAYER],
    pub pitch: [u32; MAX_PLANES_PER_LAYER],
}

/// `va_drmcommon.h` `VADRMPRIMESurfaceDescriptor`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct VaDrmPrimeSurfaceDescriptor {
    /// Surface fourcc for the DRM importer, not a layer's component format.
    pub fourcc: u32,
    pub width: u32,
    pub height: u32,
    pub num_objects: u32,
    pub objects: [VaDrmPrimeObject; MAX_OBJECTS],
    pub num_layers: u32,
    pub layers: [VaDrmPrimeLayer; MAX_LAYERS],
}

impl VaDrmPrimeSurfaceDescriptor {
    /// Zeroed for the driver to fill. Zero `num_objects`/`num_layers` is invalid:
    /// success-without-write is a [`flatten`] refusal, not an empty surface.
    pub fn zeroed() -> Self {
        Self {
            fourcc: 0,
            width: 0,
            height: 0,
            num_objects: 0,
            objects: [VaDrmPrimeObject {
                fd: -1,
                size: 0,
                drm_format_modifier: 0,
            }; MAX_OBJECTS],
            num_layers: 0,
            layers: [VaDrmPrimeLayer {
                drm_format: 0,
                num_planes: 0,
                object_index: [0; MAX_PLANES_PER_LAYER],
                offset: [0; MAX_PLANES_PER_LAYER],
                pitch: [0; MAX_PLANES_PER_LAYER],
            }; MAX_LAYERS],
        }
    }
}

/// One flattened plane. `fd` is BORROWED: several planes may share an object,
/// so the caller owns the objects and the planes only reference them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExportedPlane {
    pub fd: c_int,
    pub offset: u32,
    pub stride: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportedSurface {
    /// Descriptor top-level fourcc, never a layer's component format.
    pub fourcc: u32,
    pub width: u32,
    pub height: u32,
    /// Tiling modifier. Every object must agree ([`flatten`]).
    pub modifier: u64,
    pub planes: Vec<ExportedPlane>,
    /// Owned fds, one per object; the caller must close them.
    pub object_fds: Vec<c_int>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportError {
    /// Zero (success without a write) or past the array bound.
    ObjectCount(u32),
    LayerCount(u32),
    PlaneCount {
        layer: usize,
        planes: u32,
    },
    /// Index ≥ `num_objects` — that slot is unwritten memory.
    ObjectIndex {
        layer: usize,
        plane: usize,
        index: u32,
    },
    /// Object fd is negative; the export did not produce a usable handle.
    BadFd {
        object: usize,
        fd: c_int,
    },
    /// Objects disagree on tiling. A dmabuf import takes one modifier for the image.
    MixedModifiers {
        first: u64,
        other: u64,
    },
}

impl std::fmt::Display for ExportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExportError::ObjectCount(n) => {
                write!(
                    f,
                    "descriptor declares {n} objects (want 1..={MAX_OBJECTS})"
                )
            }
            ExportError::LayerCount(n) => {
                write!(f, "descriptor declares {n} layers (want 1..={MAX_LAYERS})")
            }
            ExportError::PlaneCount { layer, planes } => write!(
                f,
                "layer {layer} declares {planes} planes (want 1..={MAX_PLANES_PER_LAYER})"
            ),
            ExportError::ObjectIndex {
                layer,
                plane,
                index,
            } => write!(
                f,
                "layer {layer} plane {plane} names object {index}, which the descriptor \
                 does not have"
            ),
            ExportError::BadFd { object, fd } => {
                write!(f, "object {object} exported fd {fd}")
            }
            ExportError::MixedModifiers { first, other } => write!(
                f,
                "the surface's objects disagree on tiling ({first:#018x} vs {other:#018x}) — \
                 a single-modifier import cannot express it"
            ),
        }
    }
}

impl std::error::Error for ExportError {}

/// Bounds-checked walk. Takes no ownership: the caller owns the descriptor fds
/// on failure and [`ExportedSurface::object_fds`] on success. This function
/// cannot know whether the export succeeded.
pub fn flatten(desc: &VaDrmPrimeSurfaceDescriptor) -> Result<ExportedSurface, ExportError> {
    let objects = desc.num_objects as usize;
    if objects == 0 || objects > MAX_OBJECTS {
        return Err(ExportError::ObjectCount(desc.num_objects));
    }
    let layers = desc.num_layers as usize;
    if layers == 0 || layers > MAX_LAYERS {
        return Err(ExportError::LayerCount(desc.num_layers));
    }
    for (i, o) in desc.objects[..objects].iter().enumerate() {
        if o.fd < 0 {
            return Err(ExportError::BadFd {
                object: i,
                fd: o.fd,
            });
        }
    }
    let modifier = desc.objects[0].drm_format_modifier;
    if let Some(o) = desc.objects[1..objects]
        .iter()
        .find(|o| o.drm_format_modifier != modifier)
    {
        return Err(ExportError::MixedModifiers {
            first: modifier,
            other: o.drm_format_modifier,
        });
    }

    let mut planes = Vec::with_capacity(layers * 2);
    for (l, layer) in desc.layers[..layers].iter().enumerate() {
        let n = layer.num_planes as usize;
        if n == 0 || n > MAX_PLANES_PER_LAYER {
            return Err(ExportError::PlaneCount {
                layer: l,
                planes: layer.num_planes,
            });
        }
        for p in 0..n {
            let index = layer.object_index[p];
            if index as usize >= objects {
                return Err(ExportError::ObjectIndex {
                    layer: l,
                    plane: p,
                    index,
                });
            }
            planes.push(ExportedPlane {
                fd: desc.objects[index as usize].fd,
                offset: layer.offset[p],
                stride: layer.pitch[p],
            });
        }
    }

    Ok(ExportedSurface {
        fourcc: desc.fourcc,
        width: desc.width,
        height: desc.height,
        modifier,
        planes,
        object_fds: desc.objects[..objects].iter().map(|o| o.fd).collect(),
    })
}

// Layout proofs — `layout-probe.c` output, pinned.
const _: () = {
    use std::mem::align_of;
    use std::mem::offset_of;
    use std::mem::size_of;
    assert!(size_of::<VaDrmPrimeObject>() == 16);
    assert!(size_of::<VaDrmPrimeLayer>() == 56);
    assert!(size_of::<VaDrmPrimeSurfaceDescriptor>() == 312);
    assert!(align_of::<VaDrmPrimeSurfaceDescriptor>() == 8);
    assert!(offset_of!(VaDrmPrimeSurfaceDescriptor, fourcc) == 0);
    assert!(offset_of!(VaDrmPrimeSurfaceDescriptor, width) == 4);
    assert!(offset_of!(VaDrmPrimeSurfaceDescriptor, height) == 8);
    assert!(offset_of!(VaDrmPrimeSurfaceDescriptor, num_objects) == 12);
    assert!(offset_of!(VaDrmPrimeSurfaceDescriptor, objects) == 16);
    assert!(offset_of!(VaDrmPrimeSurfaceDescriptor, num_layers) == 80);
    assert!(offset_of!(VaDrmPrimeSurfaceDescriptor, layers) == 84);
};

#[cfg(test)]
mod tests {
    use super::*;

    /// Fixture-only layer formats under `SEPARATE_LAYERS`; [`flatten`] never reads them.
    const DRM_FORMAT_R8: u32 = 0x2038_5220;
    const DRM_FORMAT_GR88: u32 = 0x3838_5247;
    const MOD: u64 = 0x0200_0000_0180_1002;

    /// NV12: one object, two single-plane layers, chroma at a non-zero offset in the same BO.
    fn nv12_two_layers() -> VaDrmPrimeSurfaceDescriptor {
        let mut d = VaDrmPrimeSurfaceDescriptor::zeroed();
        d.fourcc = VA_FOURCC_NV12;
        d.width = 1920;
        d.height = 1080;
        d.num_objects = 1;
        d.objects[0] = VaDrmPrimeObject {
            fd: 7,
            size: 1920 * 1088 * 3 / 2,
            drm_format_modifier: MOD,
        };
        d.num_layers = 2;
        d.layers[0] = VaDrmPrimeLayer {
            drm_format: DRM_FORMAT_R8,
            num_planes: 1,
            object_index: [0; 4],
            offset: [0; 4],
            pitch: [1920, 0, 0, 0],
        };
        d.layers[1] = VaDrmPrimeLayer {
            drm_format: DRM_FORMAT_GR88,
            num_planes: 1,
            object_index: [0; 4],
            offset: [1920 * 1088, 0, 0, 0],
            pitch: [1920, 0, 0, 0],
        };
        d
    }

    #[test]
    fn both_layers_become_planes_and_the_surface_keeps_its_own_fourcc() {
        let out = flatten(&nv12_two_layers()).expect("a well-formed NV12 export");
        assert_eq!(out.planes.len(), 2, "chroma was dropped");
        assert_eq!(
            out.fourcc, VA_FOURCC_NV12,
            "the surface fourcc must come from the descriptor, not from layers[0].drm_format \
             ({DRM_FORMAT_R8:#010x})"
        );
        assert_ne!(out.fourcc, DRM_FORMAT_R8);
        assert_eq!(out.planes[0].offset, 0);
        assert_eq!(out.planes[1].offset, 1920 * 1088);
        // Same object, same fd; close once via `object_fds`.
        assert_eq!(out.planes[0].fd, 7);
        assert_eq!(out.planes[1].fd, 7);
        assert_eq!(out.object_fds, vec![7]);
        assert_eq!(out.modifier, MOD);
    }

    #[test]
    fn a_multi_plane_layer_flattens_in_declared_order() {
        // One layer, two planes (composed shape). The walk is shape-agnostic.
        let mut d = VaDrmPrimeSurfaceDescriptor::zeroed();
        d.fourcc = VA_FOURCC_P010;
        d.num_objects = 2;
        d.objects[0] = VaDrmPrimeObject {
            fd: 11,
            size: 64,
            drm_format_modifier: MOD,
        };
        d.objects[1] = VaDrmPrimeObject {
            fd: 12,
            size: 32,
            drm_format_modifier: MOD,
        };
        d.num_layers = 1;
        d.layers[0] = VaDrmPrimeLayer {
            drm_format: VA_FOURCC_P010,
            num_planes: 2,
            object_index: [0, 1, 0, 0],
            offset: [0, 0, 0, 0],
            pitch: [3840, 3840, 0, 0],
        };
        let out = flatten(&d).expect("a well-formed two-object export");
        assert_eq!(out.planes.len(), 2);
        assert_eq!(out.planes[0].fd, 11);
        assert_eq!(out.planes[1].fd, 12);
        assert_eq!(out.object_fds, vec![11, 12], "both objects must be closed");
    }

    #[test]
    fn a_driver_that_wrote_nothing_is_refused_rather_than_read() {
        let d = VaDrmPrimeSurfaceDescriptor::zeroed();
        assert_eq!(flatten(&d), Err(ExportError::ObjectCount(0)));
    }

    #[test]
    fn counts_past_the_arrays_are_refused_before_the_walk() {
        let mut d = nv12_two_layers();
        d.num_objects = 5;
        assert_eq!(flatten(&d), Err(ExportError::ObjectCount(5)));
        let mut d = nv12_two_layers();
        d.num_layers = 9;
        assert_eq!(flatten(&d), Err(ExportError::LayerCount(9)));
        let mut d = nv12_two_layers();
        d.layers[1].num_planes = 5;
        assert_eq!(
            flatten(&d),
            Err(ExportError::PlaneCount {
                layer: 1,
                planes: 5
            })
        );
    }

    #[test]
    fn a_plane_naming_an_object_that_does_not_exist_is_refused() {
        let mut d = nv12_two_layers();
        d.layers[1].object_index[0] = 1;
        assert_eq!(
            flatten(&d),
            Err(ExportError::ObjectIndex {
                layer: 1,
                plane: 0,
                index: 1
            })
        );
    }

    #[test]
    fn objects_that_disagree_on_tiling_are_refused_not_averaged() {
        let mut d = nv12_two_layers();
        d.num_objects = 2;
        d.objects[1] = VaDrmPrimeObject {
            fd: 8,
            size: 16,
            drm_format_modifier: 0,
        };
        d.layers[1].object_index[0] = 1;
        assert_eq!(
            flatten(&d),
            Err(ExportError::MixedModifiers {
                first: MOD,
                other: 0
            })
        );
    }

    #[test]
    fn an_object_without_an_fd_is_refused() {
        let mut d = nv12_two_layers();
        d.objects[0].fd = -1;
        assert_eq!(flatten(&d), Err(ExportError::BadFd { object: 0, fd: -1 }));
    }
}
