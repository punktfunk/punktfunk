//! GPU inventory and auto/manual preference. A preference change applies to the next session;
//! a running session keeps the GPU it opened on.

use super::shared::*;

/// Hardware GPU. Software/WARP adapters are never listed.
#[derive(Serialize, ToSchema)]
pub(crate) struct ApiGpu {
    /// `vendorid-deviceid-occurrence` (hex PCI). Stable across reboot/driver; not an index or LUID.
    #[schema(example = "10de-2c05-0")]
    id: String,
    #[schema(example = "NVIDIA GeForce RTX 5070 Ti")]
    name: String,
    /// `nvidia` | `amd` | `intel` | `other`.
    vendor: String,
    /// 0 when the platform does not expose dedicated VRAM.
    vram_mb: u64,
}

/// GPU the next session opens on. A running session keeps the GPU it already opened.
#[derive(Serialize, ToSchema)]
pub(crate) struct ApiSelectedGpu {
    id: String,
    name: String,
    /// `nvidia` | `amd` | `intel` | `other`.
    vendor: String,
    /// `preference` | `env` | `auto` | `preference_missing` (manual pick absent → auto, still stream).
    source: String,
}

/// GPU live encode sessions are on now (not the next-session pick).
#[derive(Serialize, ToSchema)]
pub(crate) struct ApiActiveGpu {
    /// Matches a `gpus` entry; empty when encoding on CPU/software.
    id: String,
    name: String,
    /// `nvidia` | `amd` | `intel` | `other`.
    vendor: String,
    /// `nvenc` | `amf` | `qsv` | `mf` | `vaapi` | `software`.
    backend: String,
    sessions: u32,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct GpuState {
    gpus: Vec<ApiGpu>,
    /// `auto` or `manual`.
    mode: String,
    /// Stored manual pick; retained in `auto` so the console can switch back. May name an absent GPU.
    preferred_id: Option<String>,
    /// Label for the stored pick, kept even when that GPU is absent.
    preferred_name: Option<String>,
    preferred_available: bool,
    /// `PUNKTFUNK_RENDER_ADAPTER` when set. Honoured in `auto`; a manual pick overrides it.
    env_override: Option<String>,
    /// `PUNKTFUNK_ENCODER` when pinned (`qsv` / `nvenc` / …, not `auto`). A vendor mismatch is
    /// overridden at session open (adapter wins); the console uses this to flag a stale pin.
    encoder_pin: Option<String>,
    selected: Option<ApiSelectedGpu>,
    active: Option<ApiActiveGpu>,
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct SetGpuPreference {
    /// `auto` (env pin, else max dedicated VRAM) or `manual`.
    #[schema(example = "manual")]
    mode: String,
    /// Required for `manual`: a currently listed GPU `id`.
    #[schema(example = "10de-2c05-0")]
    gpu_id: Option<String>,
}

pub(crate) fn gpu_state() -> GpuState {
    let gpus = pf_gpu::enumerate();
    let pref = pf_gpu::prefs().get();
    let (preferred_id, preferred_name, preferred_available) = match &pref.gpu {
        Some(want) => {
            let found = pf_gpu::find_preferred(&gpus, want);
            let id = match found {
                // Present GPU's id: identity may have matched loosely.
                Some(i) => gpus[i].id.clone(),
                None => format!(
                    "{:04x}-{:04x}-{}",
                    want.vendor_id, want.device_id, want.occurrence
                ),
            };
            let name = match found {
                Some(i) => gpus[i].name.clone(),
                None => want.name.clone(),
            };
            (Some(id), Some(name), found.is_some())
        }
        None => (None, None, false),
    };
    let selected = pf_gpu::selected_gpu().map(|sel| ApiSelectedGpu {
        vendor: sel.info.vendor_tag().into(),
        id: sel.info.id,
        name: sel.info.name,
        source: sel.source.tag().into(),
    });
    let active = pf_gpu::active().and_then(|(g, sessions)| {
        (sessions > 0).then(|| ApiActiveGpu {
            vendor: pf_gpu::vendor_tag(g.vendor_id).into(),
            id: g.id,
            name: g.name,
            backend: g.backend.into(),
            sessions,
        })
    });
    GpuState {
        gpus: gpus
            .into_iter()
            .map(|g| ApiGpu {
                vendor: g.vendor_tag().into(),
                vram_mb: g.vram_bytes / (1024 * 1024),
                id: g.id,
                name: g.name,
            })
            .collect(),
        mode: match pref.mode {
            pf_gpu::GpuMode::Auto => "auto".into(),
            pf_gpu::GpuMode::Manual => "manual".into(),
        },
        preferred_id,
        preferred_name,
        preferred_available,
        env_override: pf_host_config::config()
            .render_adapter
            .clone()
            .filter(|s| !s.is_empty()),
        encoder_pin: encoder_pin_of(&pf_host_config::config().encoder_pref),
        selected,
        active,
    }
}

/// `None` for unset, empty, and `auto` — those mean "derive from the adapter", not a pin.
fn encoder_pin_of(pref: &str) -> Option<String> {
    match pref {
        "" | "auto" => None,
        pin => Some(pin.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `auto` / empty must not surface: the console would treat the default as a stale pin.
    #[test]
    fn encoder_pin_surfaces_only_real_pins() {
        assert_eq!(encoder_pin_of(""), None);
        assert_eq!(encoder_pin_of("auto"), None);
        assert_eq!(encoder_pin_of("qsv"), Some("qsv".into()));
        assert_eq!(encoder_pin_of("software"), Some("software".into()));
    }
}

/// GPU inventory and selection
///
/// Preference changes apply to the next session; a running session keeps its GPU.
#[utoipa::path(
    get,
    path = "/gpus",
    tag = "gpu",
    operation_id = "listGpus",
    responses(
        (status = OK, description = "GPU inventory + selection state", body = GpuState),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn list_gpus() -> Json<GpuState> {
    Json(gpu_state())
}

/// Set the GPU preference
///
/// `auto` = env pin else max VRAM; `manual` pins capture+encode. Applies to the next session.
/// An absent preferred GPU falls back to auto rather than failing the session.
#[utoipa::path(
    put,
    path = "/gpus/preference",
    tag = "gpu",
    operation_id = "setGpuPreference",
    request_body = SetGpuPreference,
    responses(
        (status = OK, description = "Preference stored; the new selection state", body = GpuState),
        (status = BAD_REQUEST, description = "Unknown mode, or `gpu_id` missing / not a listed GPU", body = ApiError),
        (status = INTERNAL_SERVER_ERROR, description = "Preference could not be persisted", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn set_gpu_preference(ApiJson(req): ApiJson<SetGpuPreference>) -> Response {
    let pref = match req.mode.to_ascii_lowercase().as_str() {
        "auto" => {
            // Keep the stored manual pick so the console can offer switching back to it.
            let mut p = pf_gpu::prefs().get();
            p.mode = pf_gpu::GpuMode::Auto;
            p
        }
        "manual" => {
            let Some(id) = req
                .gpu_id
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            else {
                return api_error(StatusCode::BAD_REQUEST, "mode `manual` requires `gpu_id`");
            };
            let Some(g) = pf_gpu::enumerate().into_iter().find(|g| g.id == id) else {
                return api_error(
                    StatusCode::BAD_REQUEST,
                    "gpu_id does not match a present GPU (see GET /gpus)",
                );
            };
            pf_gpu::GpuPreference {
                mode: pf_gpu::GpuMode::Manual,
                gpu: Some(pf_gpu::PreferredGpu {
                    vendor_id: g.vendor_id,
                    device_id: g.device_id,
                    occurrence: g.occurrence,
                    name: g.name,
                }),
            }
        }
        other => {
            return api_error(
                StatusCode::BAD_REQUEST,
                &format!("unknown mode {other:?} — use `auto` or `manual`"),
            )
        }
    };
    if let Err(e) = pf_gpu::prefs().set(pref) {
        return api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("Couldn't save the GPU choice — {e:#}"),
        );
    }
    tracing::info!(mode = %req.mode, gpu_id = ?req.gpu_id, "management API: GPU preference updated");
    Json(gpu_state()).into_response()
}
