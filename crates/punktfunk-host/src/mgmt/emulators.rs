//! `/emulators`: the emulators hermir knows, holds, or finds on this host. Listing, the
//! registry, firmware status, add-ons and preparing are open to plugins; a plugin's save routes
//! answer only after the operator's one save grant. Installing, removing and adopting a copy
//! are the operator's, like every install on this host. Files cross a plugin's sandbox through
//! its own state folder. The work runs on the blocking pool: a download takes as long as it
//! takes.
use super::auth::{AuthLane, PluginIdentity};
use super::shared::*;
use crate::events::{emit, EventKind};
use axum::Extension;

/// One copy of an emulator on this host.
#[derive(Serialize, ToSchema)]
pub(crate) struct EmulatorCopy {
    /// `managed`, `flatpak`, `native` or `portable`.
    pub kind: String,
    /// The program, or `flatpak run <app id>`.
    pub exe: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_root: Option<String>,
    /// RetroArch: the libretro cores this copy has (`snes9x`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub cores: Vec<String>,
}

/// What hermir installed, and from where.
#[derive(Serialize, ToSchema)]
pub(crate) struct ManagedEmulator {
    /// `flatpak`, `github` or `url`.
    pub channel: String,
    pub exe: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub release: Option<String>,
    /// RFC 3339.
    pub installed_at: String,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct EmulatorStatus {
    pub id: String,
    pub name: String,
    /// Platform ids the emulator plays.
    pub platforms: Vec<String>,
    /// Whether this host's OS has an install channel. A detect-only entry is never offered.
    pub offered: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub managed: Option<ManagedEmulator>,
    pub detected: Vec<EmulatorCopy>,
    /// The folder a plugin is granted to reach the managed copy.
    pub home: String,
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct PrepareEmulatorRequest {
    /// The platform about to play: a catalog id like `ps2`, or an alias (RomM slug, ES-DE
    /// folder, libretro name). Without it only first-run questions are answered.
    #[serde(default)]
    pub platform: Option<String>,
    /// A folder whose files are that platform's firmware. From a plugin, a path relative to its
    /// own state directory (`firmware/ps2`); from the operator, an absolute path.
    #[serde(default)]
    pub firmware_dir: Option<String>,
}

/// One copy of the emulator, and what preparing it did.
#[derive(Serialize, ToSchema)]
pub(crate) struct PreparedCopy {
    /// The program, or `flatpak run <app id>`, as the emulator list names it.
    pub exe: String,
    pub steps: Vec<PreparedStep>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct PreparedStep {
    /// `first_run`, `firmware`, `firmware_install`, `players` or `config_root`.
    pub kind: String,
    /// The file the step is about.
    pub target: String,
    /// `applied`, `present`, `skipped` or `failed`.
    pub outcome: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct RemoveEmulatorRequest {
    /// Also delete the emulator's own data (config, saves).
    #[serde(default)]
    pub purge: bool,
}

async fn blocking<T, F>(f: F) -> Result<T, Response>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f).await.map_err(|e| {
        tracing::error!("emulator worker panicked: {e}");
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "The emulator manager stopped responding",
        )
    })
}

/// A hermir failure as an API error, by what went wrong rather than where.
pub(crate) fn hermir_err(e: &hermir::Error, what: &str) -> Response {
    let status = match e {
        hermir::Error::NotInCatalog(_) => StatusCode::NOT_FOUND,
        hermir::Error::Policy { .. } | hermir::Error::UnsupportedOs { .. } => {
            StatusCode::BAD_REQUEST
        }
        hermir::Error::Network { .. } | hermir::Error::Verify { .. } => StatusCode::BAD_GATEWAY,
        hermir::Error::Locked(_) => StatusCode::CONFLICT,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    api_error(status, &format!("{what} — {e}"))
}

fn statuses() -> hermir::Result<Vec<EmulatorStatus>> {
    let h = crate::emulators::open()?;
    let rows = h.status(None, false)?;
    Ok(rows
        .into_iter()
        .map(|s| {
            let platforms = h
                .catalog()
                .get(&s.emulator)
                .map(|e| e.platforms.clone())
                .unwrap_or_default();
            EmulatorStatus {
                home: crate::emulators::home_of(&s.emulator)
                    .to_string_lossy()
                    .into_owned(),
                id: s.emulator,
                name: s.name,
                platforms,
                offered: s.offered,
                managed: s.managed.map(|m| ManagedEmulator {
                    channel: m.channel,
                    exe: m.exe.to_string(),
                    version: m.version,
                    release: m.release,
                    installed_at: m.installed_at,
                }),
                detected: s
                    .detected
                    .into_iter()
                    .map(|d| EmulatorCopy {
                        cores: crate::emulators::cores(&h, &d),
                        kind: format!("{:?}", d.kind).to_lowercase(),
                        exe: d.exe.to_string(),
                        version: d.version,
                        config_root: d.config_root.map(|p| p.to_string_lossy().into_owned()),
                    })
                    .collect(),
            }
        })
        .collect())
}

/// List the emulators this host knows
///
/// Every catalog entry: whether this OS can install it, what hermir installed, and the copies
/// the user installed. A plugin may read this to find a managed copy's program.
#[utoipa::path(
    get,
    path = "/emulators",
    tag = "emulators",
    operation_id = "getEmulators",
    responses(
        (status = OK, description = "One row per catalog emulator", body = [EmulatorStatus]),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = INTERNAL_SERVER_ERROR, description = "The catalog or the prefix could not be read", body = ApiError),
    )
)]
pub(crate) async fn get_emulators() -> Response {
    match blocking(statuses).await {
        Ok(Ok(rows)) => Json(rows).into_response(),
        Ok(Err(e)) => hermir_err(&e, "The emulators couldn't be listed"),
        Err(r) => r,
    }
}

/// Install an emulator
///
/// Fetches the emulator through its own release channel (Flatpak on Linux, the official
/// portable build on Windows), verifies it, and places it under the host's emulator prefix.
/// Reinstalls an existing copy. Admin lane only.
#[utoipa::path(
    post,
    path = "/emulators/{id}/install",
    tag = "emulators",
    operation_id = "installEmulator",
    params(("id" = String, Path, description = "The catalog id, like `pcsx2`")),
    responses(
        (status = OK, description = "Installed", body = ManagedEmulator),
        (status = BAD_REQUEST, description = "Not offered on this OS, or never installed by policy", body = ApiError),
        (status = NOT_FOUND, description = "Not in the catalog", body = ApiError),
        (status = BAD_GATEWAY, description = "The release could not be fetched or verified", body = ApiError),
        (status = CONFLICT, description = "Another install holds the prefix", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = INTERNAL_SERVER_ERROR, description = "The files could not be placed", body = ApiError),
    )
)]
pub(crate) async fn install_emulator(Path(id): Path<String>) -> Response {
    let target = id.clone();
    match blocking(move || crate::emulators::install(&target)).await {
        Ok(Ok(row)) => {
            emit(EventKind::EmulatorsChanged { id });
            Json(ManagedEmulator {
                channel: row.channel,
                exe: row.exe.to_string(),
                version: row.version,
                release: row.release,
                installed_at: row.installed_at,
            })
            .into_response()
        }
        Ok(Err(e)) => hermir_err(&e, "The emulator didn't install"),
        Err(r) => r,
    }
}

/// Prepare an emulator for a launch
///
/// Every copy of the emulator on this host answers its first-run questions (a setup wizard, a
/// welcome box) the way clicking through would, gets the platform's firmware — copied into
/// its firmware folder, or installed by the emulator itself — and has this session's pads
/// bound in seat order, which the host undoes when the game exits. Idempotent; each step says
/// what it did, and a platform still missing its firmware says so.
#[utoipa::path(
    post,
    path = "/emulators/{id}/prepare",
    tag = "emulators",
    operation_id = "prepareEmulator",
    params(("id" = String, Path, description = "The catalog id, like `pcsx2`")),
    request_body = PrepareEmulatorRequest,
    responses(
        (status = OK, description = "What each copy's preparation did", body = [PreparedCopy]),
        (status = BAD_REQUEST, description = "A malformed platform, or a firmware folder that isn't one", body = ApiError),
        (status = FORBIDDEN, description = "The firmware folder is outside the calling plugin's state directory", body = ApiError),
        (status = NOT_FOUND, description = "Not in the catalog", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = INTERNAL_SERVER_ERROR, description = "The catalog or the firmware folder could not be read", body = ApiError),
    )
)]
pub(crate) async fn prepare_emulator(
    Path(id): Path<String>,
    Extension(lane): Extension<AuthLane>,
    who: Option<Extension<PluginIdentity>>,
    ApiJson(req): ApiJson<PrepareEmulatorRequest>,
) -> Response {
    if let Some(p) = req.platform.as_deref()
        && !valid_platform(p)
    {
        return api_error(StatusCode::BAD_REQUEST, "That isn't a platform id");
    }
    let dir = match req.firmware_dir.as_deref() {
        None => None,
        Some(d) => match state_path(
            lane,
            who.map(|Extension(w)| w.0).as_deref(),
            d,
            &plugin_states(),
            Want::Dir,
        ) {
            Ok(p) => Some(p),
            Err((status, why)) => return api_error(status, why),
        },
    };
    let platform = req.platform;
    // No session here: pads not up yet are taken as the default Xbox 360.
    let pad = punktfunk_core::config::GamepadPref::Xbox360;
    let prepared = blocking(move || {
        crate::emulators::prepare(&id, platform.as_deref(), dir.as_deref(), pad, None)
    })
    .await;
    match prepared {
        Ok(Ok(copies)) => Json(
            copies
                .into_iter()
                .map(|(exe, p)| PreparedCopy {
                    exe,
                    steps: p
                        .steps
                        .into_iter()
                        .map(|s| PreparedStep {
                            kind: s.kind,
                            target: s.target.to_string_lossy().into_owned(),
                            outcome: format!("{:?}", s.outcome).to_lowercase(),
                            note: s.note,
                        })
                        .collect(),
                })
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Ok(Err(e)) => hermir_err(&e, "The emulator wasn't prepared"),
        Err(r) => r,
    }
}

/// A platform id or alias: `ps2`, `ngc`, `Sony - PlayStation 2`.
fn valid_platform(p: &str) -> bool {
    !p.is_empty()
        && p.len() <= 64
        && p.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '-' | '_' | '.'))
}

/// What a named path must be.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Want {
    /// A folder that is there.
    Dir,
    /// A folder the host makes when missing.
    NewDir,
    /// A file that is there.
    File,
}

/// A path a caller hands the host, resolved. The operator's is absolute and may be anywhere. A
/// plugin's is relative to its own state folder, which its sandbox mounts elsewhere, and must
/// still resolve inside it: the host reads and writes only what that plugin already could.
fn state_path(
    lane: AuthLane,
    plugin: Option<&str>,
    rel: &str,
    plugin_states: &std::path::Path,
    want: Want,
) -> Result<std::path::PathBuf, (StatusCode, &'static str)> {
    let missing = (StatusCode::BAD_REQUEST, "That file or folder doesn't exist");
    let resolve = |p: &std::path::Path| {
        if want == Want::NewDir {
            // The deepest folder that is there decides where a new one would land.
            let mut base = p;
            while !base.exists() {
                base = base.parent().ok_or(missing)?;
            }
            base.canonicalize().map_err(|_| missing)?;
            std::fs::create_dir_all(p).map_err(|_| missing)?;
        }
        p.canonicalize()
            .ok()
            .filter(|p| {
                if want == Want::File {
                    p.is_file()
                } else {
                    p.is_dir()
                }
            })
            .ok_or(missing)
    };
    let path = std::path::Path::new(rel);
    if lane.is_operator() {
        return if path.is_absolute() {
            resolve(path)
        } else {
            Err(missing)
        };
    }
    let own = plugin
        .and_then(|id| plugin_states.join(id).canonicalize().ok())
        .ok_or((
            StatusCode::FORBIDDEN,
            "Handing over files needs a plugin's own token",
        ))?;
    let inside = path
        .components()
        .all(|c| matches!(c, std::path::Component::Normal(_)));
    if !inside {
        return Err((
            StatusCode::FORBIDDEN,
            "A plugin names files relative to its own state folder",
        ));
    }
    let mut base = own.join(path);
    while !base.exists() {
        base = match base.parent() {
            Some(p) => p.to_path_buf(),
            None => break,
        };
    }
    let escapes = |p: &std::path::Path| p.canonicalize().is_ok_and(|p| !p.starts_with(&own));
    if escapes(&base) {
        return Err((
            StatusCode::FORBIDDEN,
            "A plugin may hand over only files in its own state folder",
        ));
    }
    let real = resolve(&own.join(path))?;
    if !real.starts_with(&own) {
        return Err((
            StatusCode::FORBIDDEN,
            "A plugin may hand over only files in its own state folder",
        ));
    }
    Ok(real)
}

/// Remove a managed emulator
///
/// Takes the release's files away and keeps the emulator's own data unless `purge` is set.
/// A Flatpak is uninstalled. Grants on its folder stay until the operator forgets them.
#[utoipa::path(
    post,
    path = "/emulators/{id}/remove",
    tag = "emulators",
    operation_id = "removeEmulator",
    params(("id" = String, Path, description = "The catalog id")),
    request_body = RemoveEmulatorRequest,
    responses(
        (status = NO_CONTENT, description = "Removed"),
        (status = NOT_FOUND, description = "Not in the catalog", body = ApiError),
        (status = CONFLICT, description = "Another install holds the prefix", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = INTERNAL_SERVER_ERROR, description = "Not installed by the host, or the files could not be removed", body = ApiError),
    )
)]
pub(crate) async fn remove_emulator(
    Path(id): Path<String>,
    ApiJson(req): ApiJson<RemoveEmulatorRequest>,
) -> Response {
    let target = id.clone();
    match blocking(move || crate::emulators::remove(&target, req.purge)).await {
        Ok(Ok(())) => {
            emit(EventKind::EmulatorsChanged { id });
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(Err(e)) => hermir_err(&e, "The emulator wasn't removed"),
        Err(r) => r,
    }
}

/// hermir's registry, as hermir writes it; its JSON Schema is hermir's own.
#[derive(Serialize, ToSchema)]
#[schema(value_type = Object)]
pub(crate) struct EmulatorRegistry(hermir::Registry);

/// Read the emulator registry
///
/// Every platform (names, aliases, extensions, folder shape) and every emulator as a library
/// sees it: what it plays, whether this OS installs it, which platforms take saves, add-ons and
/// firmware. The facts a plugin needs to list games and pick an emulator.
#[utoipa::path(
    get,
    path = "/emulators/catalog",
    tag = "emulators",
    operation_id = "getEmulatorRegistry",
    responses(
        (status = OK, description = "The registry", body = EmulatorRegistry),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = INTERNAL_SERVER_ERROR, description = "The catalog could not be read", body = ApiError),
    )
)]
pub(crate) async fn get_emulator_registry() -> Response {
    match blocking(crate::emulators::registry).await {
        Ok(Ok(r)) => Json(EmulatorRegistry(r)).into_response(),
        Ok(Err(e)) => hermir_err(&e, "The registry couldn't be read"),
        Err(r) => r,
    }
}

#[derive(Deserialize)]
pub(crate) struct PlatformQuery {
    /// A catalog platform id or alias, like `ps2`.
    pub platform: String,
    /// The game's folder, for an emulator that keeps saves beside its games.
    #[serde(default)]
    pub game: Option<String>,
}

/// One platform's firmware on the best copy.
#[derive(Serialize, ToSchema)]
pub(crate) struct FirmwareStatusView {
    pub platform: String,
    /// The need is met.
    pub ok: bool,
    /// What the platform needs, in a phrase a UI shows.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub optional: bool,
    /// Accepted file names.
    pub any_of: Vec<String>,
    pub found: Vec<FirmwareFileView>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct FirmwareFileView {
    /// The file's name.
    pub name: String,
    pub md5: String,
    /// `true` a known good dump, `false` named right but not one, absent when no hashes are known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub known: Option<bool>,
}

/// Check a platform's firmware
///
/// Whether the best copy of the emulator has the platform's firmware, each file hashed against
/// the good dumps the catalog knows. `null` when the platform needs none.
#[utoipa::path(
    get,
    path = "/emulators/{id}/firmware",
    tag = "emulators",
    operation_id = "getEmulatorFirmware",
    params(
        ("id" = String, Path, description = "The catalog id"),
        ("platform" = String, Query, description = "A catalog platform id or alias, like `ps2`"),
    ),
    responses(
        (status = OK, description = "The platform's firmware, or null", body = Option<FirmwareStatusView>),
        (status = BAD_REQUEST, description = "A malformed platform, or no copy on this host", body = ApiError),
        (status = NOT_FOUND, description = "Not in the catalog", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn get_emulator_firmware(
    Path(id): Path<String>,
    Query(q): Query<PlatformQuery>,
) -> Response {
    if !valid_platform(&q.platform) {
        return api_error(StatusCode::BAD_REQUEST, "That isn't a platform id");
    }
    match blocking(move || crate::emulators::firmware_status(&id, &q.platform)).await {
        Ok(Ok(status)) => Json(status.map(|s| {
            FirmwareStatusView {
                platform: s.platform,
                ok: s.ok,
                note: s.need.note,
                optional: s.need.optional,
                any_of: s.need.any_of,
                found: s
                    .found
                    .into_iter()
                    .map(|f| FirmwareFileView {
                        name: f
                            .path
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default(),
                        md5: f.md5,
                        known: f.known,
                    })
                    .collect(),
            }
        }))
        .into_response(),
        Ok(Err(e)) => hermir_err(&e, "The firmware couldn't be checked"),
        Err(r) => r,
    }
}

/// One save unit on the best copy.
#[derive(Serialize, ToSchema)]
pub(crate) struct SaveUnitView {
    /// `save`, `memcard` or `state`.
    pub kind: String,
    /// The same on every machine; `.tar` ends a folder's.
    pub name: String,
    /// It holds every game's data at once (a shared memory card).
    pub shared: bool,
    /// The emulator writes an empty one when a game first starts.
    pub written_at_start: bool,
    pub size: u64,
    pub files: u32,
    /// Its newest change, milliseconds since 1970.
    pub modified: u64,
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct UnitRef {
    /// `save`, `memcard` or `state`.
    #[schema(value_type = String)]
    pub kind: hermir::SaveKind,
    pub name: String,
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct ExportUnitsRequest {
    pub platform: String,
    #[serde(default)]
    pub game: Option<String>,
    pub units: Vec<UnitRef>,
    /// The folder the files go to: relative to a plugin's own state folder, absolute for the
    /// operator. Made when missing.
    pub dir: String,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct ExportedUnitView {
    pub kind: String,
    pub name: String,
    /// The file written, spelled the way the request spelled `dir`.
    pub file: String,
    pub size: u64,
    /// MD5, hex.
    pub md5: String,
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct ImportUnit {
    #[schema(value_type = String)]
    pub kind: hermir::SaveKind,
    pub name: String,
    /// The file to put back: relative to a plugin's own state folder, absolute for the operator.
    pub file: String,
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct ImportUnitsRequest {
    pub platform: String,
    #[serde(default)]
    pub game: Option<String>,
    /// Rows of one name go to the first of their kinds with a place for it.
    pub units: Vec<ImportUnit>,
    /// The names a server holds for the same game.
    #[serde(default)]
    pub others: Vec<String>,
}

/// What one save or add-on step did.
#[derive(Serialize, ToSchema)]
pub(crate) struct EmulatorStep {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    pub target: String,
    /// `applied`, `present`, `skipped`, `conflict` or `failed`.
    pub outcome: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct ContentRequest {
    pub platform: String,
    /// `update` or `dlc`.
    pub kind: String,
    /// The add-on files: absolute under a folder the plugin was granted, or relative to its own
    /// state folder.
    pub files: Vec<String>,
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct AdoptRequest {
    /// The copy's program, absolute.
    pub exe: String,
    /// Forget the copy instead.
    #[serde(default)]
    pub forget: bool,
}

/// A hermir enum as its JSON spelling: `applied`, `memcard`.
fn wire<T: Serialize>(v: T) -> String {
    serde_json::to_value(v)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

/// Who may move saves: the operator, or a plugin holding the save grant.
fn may_move_saves(
    st: &MgmtState,
    lane: AuthLane,
    plugin: Option<&str>,
) -> Result<(), (StatusCode, &'static str)> {
    if lane.is_operator() {
        return Ok(());
    }
    let grant = crate::emulators::saves_grant();
    let held = plugin.is_some_and(|id| {
        st.access
            .grants_for(id)
            .iter()
            .any(|g| std::path::Path::new(&g.path) == grant)
    });
    if held {
        Ok(())
    } else {
        Err((
            StatusCode::FORBIDDEN,
            "Moving saves needs the operator's yes — ask with `saves: true` on an access request",
        ))
    }
}

/// A game folder the caller names. A plugin's must sit under a root it may reach.
fn game_dir(
    lane: AuthLane,
    plugin: Option<&str>,
    game: Option<&str>,
) -> Result<Option<std::path::PathBuf>, (StatusCode, &'static str)> {
    let Some(game) = game else { return Ok(None) };
    let path = std::path::PathBuf::from(game);
    let ok = path.is_absolute()
        && (lane.is_operator()
            || plugin
                .and_then(crate::plugins::manifest::for_provider)
                .is_some_and(|m| m.confines(&path)));
    if ok {
        Ok(Some(path))
    } else {
        Err((
            StatusCode::FORBIDDEN,
            "The game folder is outside the folders this plugin may reach",
        ))
    }
}

fn plugin_states() -> std::path::PathBuf {
    pf_paths::config_dir().join("plugin-state")
}

/// List a platform's saves
///
/// The save units of the platform on the best copy of the emulator: name, kind, size and a
/// stamp that changes with the save. A plugin needs the operator's save grant.
#[utoipa::path(
    get,
    path = "/emulators/{id}/saves",
    tag = "emulators",
    operation_id = "getEmulatorSaves",
    params(
        ("id" = String, Path, description = "The catalog id"),
        ("platform" = String, Query, description = "A catalog platform id or alias, like `ps2`"),
        ("game" = Option<String>, Query, description = "The game's folder, for an emulator that keeps saves beside its games"),
    ),
    responses(
        (status = OK, description = "The save units", body = [SaveUnitView]),
        (status = BAD_REQUEST, description = "A malformed platform, or no copy on this host", body = ApiError),
        (status = FORBIDDEN, description = "No save grant, or a game folder outside the plugin's reach", body = ApiError),
        (status = NOT_FOUND, description = "Not in the catalog", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn get_emulator_saves(
    State(st): State<Arc<MgmtState>>,
    Path(id): Path<String>,
    Extension(lane): Extension<AuthLane>,
    who: Option<Extension<PluginIdentity>>,
    Query(q): Query<PlatformQuery>,
) -> Response {
    let plugin = who.map(|Extension(w)| w.0);
    if let Err((status, why)) = may_move_saves(&st, lane, plugin.as_deref()) {
        return api_error(status, why);
    }
    if !valid_platform(&q.platform) {
        return api_error(StatusCode::BAD_REQUEST, "That isn't a platform id");
    }
    let game = match game_dir(lane, plugin.as_deref(), q.game.as_deref()) {
        Ok(g) => g,
        Err((status, why)) => return api_error(status, why),
    };
    match blocking(move || crate::emulators::units(&id, &q.platform, game.as_deref())).await {
        Ok(Ok(units)) => Json(
            units
                .into_iter()
                .map(|u| SaveUnitView {
                    kind: wire(u.kind),
                    name: u.name,
                    shared: u.shared,
                    written_at_start: u.written_at_start,
                    size: u.size,
                    files: u.files,
                    modified: u.modified,
                })
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Ok(Err(e)) => hermir_err(&e, "The saves couldn't be listed"),
        Err(r) => r,
    }
}

/// Copy saves out
///
/// Writes each named unit into `dir`: the save file, or a tar of a save folder with nothing in
/// its headers that differs between machines. A plugin's `dir` is inside its own state folder.
#[utoipa::path(
    post,
    path = "/emulators/{id}/saves/export",
    tag = "emulators",
    operation_id = "exportEmulatorSaves",
    params(("id" = String, Path, description = "The catalog id")),
    request_body = ExportUnitsRequest,
    responses(
        (status = OK, description = "The files written", body = [ExportedUnitView]),
        (status = BAD_REQUEST, description = "A malformed platform or unit, or no copy on this host", body = ApiError),
        (status = FORBIDDEN, description = "No save grant, or a folder outside the plugin's reach", body = ApiError),
        (status = NOT_FOUND, description = "Not in the catalog", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn export_emulator_saves(
    State(st): State<Arc<MgmtState>>,
    Path(id): Path<String>,
    Extension(lane): Extension<AuthLane>,
    who: Option<Extension<PluginIdentity>>,
    ApiJson(req): ApiJson<ExportUnitsRequest>,
) -> Response {
    let plugin = who.map(|Extension(w)| w.0);
    if let Err((status, why)) = may_move_saves(&st, lane, plugin.as_deref()) {
        return api_error(status, why);
    }
    if !valid_platform(&req.platform) {
        return api_error(StatusCode::BAD_REQUEST, "That isn't a platform id");
    }
    let game = match game_dir(lane, plugin.as_deref(), req.game.as_deref()) {
        Ok(g) => g,
        Err((status, why)) => return api_error(status, why),
    };
    let out = match state_path(
        lane,
        plugin.as_deref(),
        &req.dir,
        &plugin_states(),
        Want::NewDir,
    ) {
        Ok(p) => p,
        Err((status, why)) => return api_error(status, why),
    };
    let units: Vec<(hermir::SaveKind, String)> =
        req.units.into_iter().map(|u| (u.kind, u.name)).collect();
    let dir = req.dir;
    let exported = blocking(move || {
        crate::emulators::export_units(&id, &req.platform, game.as_deref(), &units, &out)
    })
    .await;
    match exported {
        Ok(Ok(rows)) => Json(
            rows.into_iter()
                .map(|(kind, u)| ExportedUnitView {
                    kind: wire(kind),
                    file: u
                        .file
                        .file_name()
                        .map(|n| {
                            std::path::Path::new(&dir)
                                .join(n)
                                .to_string_lossy()
                                .into_owned()
                        })
                        .unwrap_or_default(),
                    name: u.name,
                    size: u.size,
                    md5: u.md5,
                })
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Ok(Err(e)) => hermir_err(&e, "The saves weren't copied out"),
        Err(r) => r,
    }
}

/// Put saves back
///
/// Each unit goes where the emulator keeps it, in the first of its kinds with a place for its
/// name; what was there is kept in hermir's save backups. A unit with no place yet is a step
/// that says so. A plugin's files are inside its own state folder.
#[utoipa::path(
    post,
    path = "/emulators/{id}/saves/import",
    tag = "emulators",
    operation_id = "importEmulatorSaves",
    params(("id" = String, Path, description = "The catalog id")),
    request_body = ImportUnitsRequest,
    responses(
        (status = OK, description = "One step per unit", body = [EmulatorStep]),
        (status = BAD_REQUEST, description = "A malformed platform or unit, a missing file, or no copy on this host", body = ApiError),
        (status = FORBIDDEN, description = "No save grant, or a file outside the plugin's reach", body = ApiError),
        (status = NOT_FOUND, description = "Not in the catalog", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn import_emulator_saves(
    State(st): State<Arc<MgmtState>>,
    Path(id): Path<String>,
    Extension(lane): Extension<AuthLane>,
    who: Option<Extension<PluginIdentity>>,
    ApiJson(req): ApiJson<ImportUnitsRequest>,
) -> Response {
    let plugin = who.map(|Extension(w)| w.0);
    if let Err((status, why)) = may_move_saves(&st, lane, plugin.as_deref()) {
        return api_error(status, why);
    }
    if !valid_platform(&req.platform) {
        return api_error(StatusCode::BAD_REQUEST, "That isn't a platform id");
    }
    let game = match game_dir(lane, plugin.as_deref(), req.game.as_deref()) {
        Ok(g) => g,
        Err((status, why)) => return api_error(status, why),
    };
    // One row per name, its kinds in the order given.
    let mut rows: Vec<(String, std::path::PathBuf, Vec<hermir::SaveKind>)> = Vec::new();
    for u in req.units {
        if let Some(row) = rows.iter_mut().find(|r| r.0 == u.name) {
            row.2.push(u.kind);
            continue;
        }
        match state_path(
            lane,
            plugin.as_deref(),
            &u.file,
            &plugin_states(),
            Want::File,
        ) {
            Ok(file) => rows.push((u.name, file, vec![u.kind])),
            Err((status, why)) => return api_error(status, why),
        }
    }
    let others = req.others;
    let platform = req.platform;
    let imported = blocking(move || {
        rows.into_iter()
            .map(|(name, file, kinds)| {
                let step = crate::emulators::import_unit(
                    &id,
                    &platform,
                    game.as_deref(),
                    &kinds,
                    &name,
                    &file,
                    &others,
                )?;
                Ok(EmulatorStep {
                    name: Some(name),
                    source: None,
                    target: step.target.to_string_lossy().into_owned(),
                    outcome: wire(step.outcome),
                    note: step.note,
                })
            })
            .collect::<hermir::Result<Vec<_>>>()
    })
    .await;
    match imported {
        Ok(Ok(steps)) => Json(steps).into_response(),
        Ok(Err(e)) => hermir_err(&e, "The saves weren't put back"),
        Err(r) => r,
    }
}

/// Install a game's update or DLC
///
/// Hands the files to the best copy of the emulator the way it takes them: a folder it scans,
/// its own installer, or a registration file. One step per file. An emulator with no way says
/// why.
#[utoipa::path(
    post,
    path = "/emulators/{id}/content",
    tag = "emulators",
    operation_id = "installEmulatorContent",
    params(("id" = String, Path, description = "The catalog id")),
    request_body = ContentRequest,
    responses(
        (status = OK, description = "One step per file", body = [EmulatorStep]),
        (status = BAD_REQUEST, description = "A malformed platform or kind, no way to install it, or no copy on this host", body = ApiError),
        (status = FORBIDDEN, description = "A file outside the plugin's reach", body = ApiError),
        (status = NOT_FOUND, description = "Not in the catalog", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn install_emulator_content(
    Path(id): Path<String>,
    Extension(lane): Extension<AuthLane>,
    who: Option<Extension<PluginIdentity>>,
    ApiJson(req): ApiJson<ContentRequest>,
) -> Response {
    if !valid_platform(&req.platform) {
        return api_error(StatusCode::BAD_REQUEST, "That isn't a platform id");
    }
    if !matches!(req.kind.as_str(), "update" | "dlc") {
        return api_error(StatusCode::BAD_REQUEST, "The kind is `update` or `dlc`");
    }
    let plugin = who.map(|Extension(w)| w.0);
    let manifest = plugin
        .as_deref()
        .and_then(crate::plugins::manifest::for_provider);
    let mut files = Vec::with_capacity(req.files.len());
    for f in &req.files {
        let path = std::path::Path::new(f);
        let file = if path.is_absolute()
            && (lane.is_operator() || manifest.as_ref().is_some_and(|m| m.confines(path)))
        {
            Ok(path.to_path_buf())
        } else {
            state_path(lane, plugin.as_deref(), f, &plugin_states(), Want::File)
        };
        match file {
            Ok(p) => files.push(p),
            Err((status, why)) => return api_error(status, why),
        }
    }
    let installed =
        blocking(move || crate::emulators::install_content(&id, &req.platform, &req.kind, &files))
            .await;
    match installed {
        Ok(Ok(steps)) => Json(
            steps
                .into_iter()
                .map(|s| EmulatorStep {
                    name: None,
                    source: Some(s.source.to_string_lossy().into_owned()),
                    target: s.target.to_string_lossy().into_owned(),
                    outcome: wire(s.outcome),
                    note: s.note,
                })
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Ok(Err(e)) => hermir_err(&e, "The add-ons weren't installed"),
        Err(r) => r,
    }
}

/// Adopt a copy of an emulator
///
/// Points the host at the operator's own copy, one no rule finds (a portable build unpacked
/// anywhere), so it is listed and launched like any other; `forget` drops it again. Admin lane
/// only.
#[utoipa::path(
    post,
    path = "/emulators/{id}/adopt",
    tag = "emulators",
    operation_id = "adoptEmulator",
    params(("id" = String, Path, description = "The catalog id")),
    request_body = AdoptRequest,
    responses(
        (status = OK, description = "Adopted, or forgotten", body = Option<EmulatorCopy>),
        (status = BAD_REQUEST, description = "Not a program on this host", body = ApiError),
        (status = NOT_FOUND, description = "Not in the catalog", body = ApiError),
        (status = CONFLICT, description = "Another install holds the prefix", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn adopt_emulator(
    Path(id): Path<String>,
    ApiJson(req): ApiJson<AdoptRequest>,
) -> Response {
    let target = id.clone();
    let adopted = blocking(move || {
        crate::emulators::adopt(&target, std::path::Path::new(&req.exe), !req.forget)
    })
    .await;
    match adopted {
        Ok(Ok(copy)) => {
            emit(EventKind::EmulatorsChanged { id });
            Json(copy.map(|d| EmulatorCopy {
                kind: format!("{:?}", d.kind).to_lowercase(),
                exe: d.exe.to_string(),
                version: d.version,
                config_root: d.config_root.map(|p| p.to_string_lossy().into_owned()),
                cores: Vec::new(),
            }))
            .into_response()
        }
        Ok(Err(e)) => hermir_err(&e, "The copy wasn't adopted"),
        Err(r) => r,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plugin_hands_over_firmware_only_from_its_own_state_folder() {
        let states = tempfile::tempdir().unwrap();
        let own = states.path().join("rom-manager");
        std::fs::create_dir_all(own.join("firmware/ps2")).unwrap();
        std::fs::create_dir_all(states.path().join("other/firmware")).unwrap();
        let plugin = |dir: &str| {
            state_path(
                AuthLane::Plugin,
                Some("rom-manager"),
                dir,
                states.path(),
                Want::Dir,
            )
            .map_err(|(s, _)| s)
        };
        assert_eq!(
            plugin("firmware/ps2").unwrap(),
            own.join("firmware/ps2").canonicalize().unwrap()
        );
        assert_eq!(plugin("../other/firmware"), Err(StatusCode::FORBIDDEN));
        let absolute = own.join("firmware/ps2").to_string_lossy().into_owned();
        assert_eq!(plugin(&absolute), Err(StatusCode::FORBIDDEN));
        assert_eq!(plugin("firmware/none"), Err(StatusCode::BAD_REQUEST));
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(states.path().join("other"), own.join("out")).unwrap();
            assert_eq!(plugin("out/firmware"), Err(StatusCode::FORBIDDEN));
        }
        let shared = state_path(
            AuthLane::Plugin,
            None,
            "firmware/ps2",
            states.path(),
            Want::Dir,
        );
        assert_eq!(shared.map_err(|(s, _)| s), Err(StatusCode::FORBIDDEN));
        let operator = state_path(AuthLane::Admin, None, &absolute, states.path(), Want::Dir);
        assert!(operator.is_ok());
    }

    #[test]
    fn a_plugin_gets_a_new_folder_only_inside_its_own_state_folder() {
        let states = tempfile::tempdir().unwrap();
        let own = states.path().join("rom-manager");
        std::fs::create_dir_all(&own).unwrap();
        std::fs::write(own.join("save.srm"), b"x").unwrap();
        let plugin = |rel: &str, want| {
            state_path(
                AuthLane::Plugin,
                Some("rom-manager"),
                rel,
                states.path(),
                want,
            )
            .map_err(|(s, _)| s)
        };
        let out = plugin("saves/out", Want::NewDir).unwrap();
        assert!(out.is_dir() && out.ends_with("saves/out"));
        assert!(plugin("save.srm", Want::File).is_ok());
        assert_eq!(
            plugin("saves/out", Want::File),
            Err(StatusCode::BAD_REQUEST)
        );
        assert_eq!(
            plugin("../escape", Want::NewDir),
            Err(StatusCode::FORBIDDEN)
        );
        assert!(!states.path().join("escape").exists());
        #[cfg(unix)]
        {
            let elsewhere = tempfile::tempdir().unwrap();
            std::os::unix::fs::symlink(elsewhere.path(), own.join("link")).unwrap();
            assert_eq!(plugin("link/new", Want::NewDir), Err(StatusCode::FORBIDDEN));
            assert!(!elsewhere.path().join("new").exists());
        }
    }
}
