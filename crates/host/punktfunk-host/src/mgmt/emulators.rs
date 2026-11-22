//! `/emulators`: the emulators hermir knows, holds, or finds on this host. Listing, the
//! registry, firmware status, add-ons and preparing are open to plugins; a plugin's save routes
//! answer only after the operator's one save grant. Installing, removing and adopting a copy
//! are the operator's, like every install on this host. Files cross a plugin's sandbox through
//! its own state folder. On the host with the plugins, `?profile=` points the save and firmware
//! routes at that profile's seat ([`crate::emulators::exchange`]). The work runs on the blocking
//! pool: a download takes as long as it takes.
use super::auth::{AuthLane, PluginIdentity};
use super::shared::*;
use crate::emulators::handover;
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
    /// hermir's name for the step, like `first_run`, `firmware`, `firmware_install`,
    /// `config_root`, or `config` for the session's pads.
    pub kind: String,
    /// The file the step is about.
    pub target: String,
    /// `applied`, `present`, `skipped`, `conflict` or `failed`.
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

/// A hermir failure as an API error, by what went wrong rather than where. `what` is the sentence
/// for a person, the English of the host message `apierr_<code>`; the error goes to the log.
pub(crate) fn hermir_err(e: &hermir::Error, (code, what): (&str, &str)) -> Response {
    api_error_logged(hermir_status(e), code, what, e)
}

/// [`hermir_err`] for a sentence with no host message: the error rides along.
pub(crate) fn hermir_plain(e: &hermir::Error, what: &str) -> Response {
    api_error(hermir_status(e), &format!("{what} — {e}"))
}

fn hermir_status(e: &hermir::Error) -> StatusCode {
    match e {
        hermir::Error::NotInCatalog(_) => StatusCode::NOT_FOUND,
        hermir::Error::Policy { .. } | hermir::Error::UnsupportedOs { .. } => {
            StatusCode::BAD_REQUEST
        }
        hermir::Error::Network { .. } | hermir::Error::Verify { .. } => StatusCode::BAD_GATEWAY,
        hermir::Error::Locked(_) => StatusCode::CONFLICT,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    }
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
    match blocking("emulator_manager", statuses).await {
        Ok(Ok(rows)) => Json(rows).into_response(),
        Ok(Err(e)) => hermir_err(
            &e,
            ("emulators_not_listed", "The emulators couldn't be listed"),
        ),
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
    match blocking("emulator_manager", move || {
        crate::emulators::install(&target)
    })
    .await
    {
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
        Ok(Err(e)) => hermir_err(
            &e,
            ("emulator_not_installed", "The emulator didn't install"),
        ),
        Err(r) => r,
    }
}

/// Prepare an emulator for a launch
///
/// Every copy of the emulator on this host answers its first-run questions (a setup wizard, a
/// welcome box) the way clicking through would, gets the platform's firmware — copied into
/// its firmware folder, or installed by the emulator itself — and has this session's pads
/// bound in seat order, which the host undoes when the game exits. Idempotent; each step says
/// what it did, and a platform still missing its firmware says so. A Windows seat host takes it
/// from the operator alone.
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
        (status = FORBIDDEN, description = "The firmware folder is outside the calling plugin's state directory, or a Windows seat host's plugin asks", body = ApiError),
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
    if let Some(refused) = seat_write_refusal(lane, crate::plugins::system_seat_host()) {
        return refused;
    }
    if let Some(p) = req.platform.as_deref()
        && !valid_platform(p)
    {
        return api_error(StatusCode::BAD_REQUEST, "That isn't a platform id");
    }
    let plugin = who.map(|Extension(w)| w.0);
    let firmware = match req.firmware_dir.as_deref() {
        None => None,
        Some(d) => match folder(lane, plugin.as_deref(), d, &plugin_states(), false) {
            Ok(f) => Some(f),
            Err((status, why)) => return api_error(status, why),
        },
    };
    let platform = req.platform;
    // No session here: pads not up yet are taken as the default Xbox 360.
    let pad = punktfunk_core::config::GamepadPref::Xbox360;
    let prepared = blocking("emulator_manager", move || {
        // hermir reads a copy of a plugin's firmware, never its folder.
        let staged = match &firmware {
            Some(Folder::Held(dir)) => Some(
                handover::staged(dir).map_err(|e| moved("copy", &handover::scratch_base(), e))?,
            ),
            _ => None,
        };
        let dir = match &firmware {
            Some(Folder::Path(p)) => Some(p.as_path()),
            _ => staged.as_ref().map(handover::Scratch::path),
        };
        crate::emulators::prepare(&id, platform.as_deref(), dir, pad)
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
        Ok(Err(e)) => hermir_err(
            &e,
            ("emulator_not_prepared", "The emulator wasn't prepared"),
        ),
        Err(r) => r,
    }
}

/// The emulator file routes a Windows seat host (`seat`) takes from the operator alone, the save
/// list included: it is SYSTEM, and a plugin's state or game folder is the seat account's to
/// point anywhere. Its emulators are the box's copies.
fn seat_write_refusal(lane: AuthLane, seat: bool) -> Option<Response> {
    (seat && !lane.is_operator()).then(|| {
        let (code, why) = super::own_account::EMULATORS_YET;
        api_error_coded(StatusCode::FORBIDDEN, code, why)
    })
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

/// Why a handed path is refused.
type Refusal = (StatusCode, &'static str);
const MISSING: Refusal = (StatusCode::BAD_REQUEST, "That file or folder doesn't exist");
const NO_OWN_TOKEN: Refusal = (
    StatusCode::FORBIDDEN,
    "Handing over files needs a plugin's own token",
);
const NOT_RELATIVE: Refusal = (
    StatusCode::FORBIDDEN,
    "A plugin names files relative to its own state folder",
);
const OUTSIDE_STATE: Refusal = (
    StatusCode::FORBIDDEN,
    "A plugin may hand over only files in its own state folder",
);
const NOT_COPIED: Refusal = (
    StatusCode::INTERNAL_SERVER_ERROR,
    "The host couldn't read or copy that file",
);

/// A path a caller hands the host, resolved. The operator's is absolute and may be anywhere. A
/// plugin's is relative to its own state folder, which its sandbox mounts elsewhere, and must
/// still resolve inside it: the host reads and writes only what that plugin already could. What
/// the host itself opens of a plugin's goes through [`plugin_dir`] instead.
fn state_path(
    lane: AuthLane,
    plugin: Option<&str>,
    rel: &str,
    plugin_states: &std::path::Path,
    want: Want,
) -> Result<std::path::PathBuf, Refusal> {
    let missing = MISSING;
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
    let own = own_state(plugin, plugin_states, path)?;
    let mut base = own.join(path);
    while !base.exists() {
        base = match base.parent() {
            Some(p) => p.to_path_buf(),
            None => break,
        };
    }
    let escapes = |p: &std::path::Path| p.canonicalize().is_ok_and(|p| !p.starts_with(&own));
    if escapes(&base) {
        return Err(OUTSIDE_STATE);
    }
    let real = resolve(&own.join(path))?;
    if !real.starts_with(&own) {
        return Err(OUTSIDE_STATE);
    }
    Ok(real)
}

/// Plugin `plugin`'s own state folder, canonical, when `rel` is a plain path below it. Only the
/// person can make that folder a link, since no sandbox binds the state root, so a state folder
/// moved elsewhere by a link is followed.
fn own_state(
    plugin: Option<&str>,
    plugin_states: &std::path::Path,
    rel: &std::path::Path,
) -> Result<std::path::PathBuf, Refusal> {
    let own = plugin
        .and_then(|id| plugin_states.join(id).canonicalize().ok())
        .filter(|own| own.is_dir())
        .ok_or(NO_OWN_TOKEN)?;
    let inside = rel
        .components()
        .all(|c| matches!(c, std::path::Component::Normal(_)));
    if inside {
        Ok(own)
    } else {
        Err(NOT_RELATIVE)
    }
}

/// The folder `rel` below plugin `plugin`'s own state folder, held open ([`handover::Dir`]) and
/// made when `make` and missing. The host reads and writes there only through it: a link the
/// plugin swaps in after this check reaches nothing.
fn plugin_dir(
    plugin: Option<&str>,
    rel: &std::path::Path,
    plugin_states: &std::path::Path,
    make: bool,
) -> Result<handover::Dir, Refusal> {
    let own = own_state(plugin, plugin_states, rel)?;
    handover::Dir::open(&own, rel, make).map_err(refusal)
}

/// A game file or folder a plugin hands to an emulator's add-on install, as hermir may open it.
/// In a root it may only read, its real path. In its state folder or a root it may write:
/// with `copy`, a scratch folder, a [`handover::mirror`] of it there, so nothing the plugin
/// swaps in later is read; without, for an emulator that keeps the path, the file itself,
/// checked and held in `held` until the install ends ([`handover::hold`]).
fn plugin_content(
    plugin: Option<&str>,
    manifest: Option<&crate::plugins::manifest::PluginManifest>,
    path: &std::path::Path,
    plugin_states: &std::path::Path,
    copy: Option<&std::path::Path>,
    held: &mut Vec<handover::Held>,
) -> Result<std::path::PathBuf, Refusal> {
    let id = plugin.ok_or(NO_OWN_TOKEN)?;
    let mut place = |root: &std::path::Path, rel: &std::path::Path, part: std::path::PathBuf| {
        let Some(copy) = copy else {
            let file = handover::hold(root, rel).map_err(refusal)?;
            let path = file.path().to_path_buf();
            held.push(file);
            return Ok(path);
        };
        handover::mirror(root, rel, &copy.join(id).join(part)).map_err(refusal)
    };
    if !path.is_absolute() {
        let own = own_state(plugin, plugin_states, path)?;
        return place(&own, path, "state".into());
    }
    let reach = |write| manifest.and_then(|m| m.reaches(path, write));
    if let Some((root, real)) = reach(true) {
        let rel = real.strip_prefix(&root).map_err(|_| OUTSIDE_STATE)?;
        let digest = <sha2::Sha256 as sha2::Digest>::digest(root.as_os_str().as_encoded_bytes());
        let part = std::path::Path::new("roots").join(hex::encode(&digest[..8]));
        return place(&root, rel, part);
    }
    reach(false).map(|(_, real)| real).ok_or(NOT_RELATIVE)
}

/// A handover's failure as the caller reads it: a path that isn't there or isn't the kind
/// asked for is missing, and a link leads out of the state folder. Anything else is the host
/// failing to read or copy, a full disk or a locked file, and goes to the log with its cause.
fn refusal(e: std::io::Error) -> Refusal {
    use std::io::ErrorKind;
    // A link met where none is followed.
    #[cfg(unix)]
    if e.raw_os_error() == Some(libc::ELOOP) {
        return OUTSIDE_STATE;
    }
    match e.kind() {
        ErrorKind::NotFound | ErrorKind::NotADirectory | ErrorKind::InvalidInput => MISSING,
        // The handover's own refusal carries no OS code; the OS refusing a read does.
        ErrorKind::PermissionDenied if e.raw_os_error().is_none() => OUTSIDE_STATE,
        _ => {
            tracing::warn!(error = %e, "plugin file not read or copied");
            NOT_COPIED
        }
    }
}

/// A folder a caller names: the operator's path ([`state_path`]), or a plugin's held open below
/// its state folder ([`plugin_dir`]).
enum Folder {
    Path(std::path::PathBuf),
    Held(handover::Dir),
}

fn folder(
    lane: AuthLane,
    plugin: Option<&str>,
    rel: &str,
    plugin_states: &std::path::Path,
    make: bool,
) -> Result<Folder, Refusal> {
    if lane.is_operator() {
        let want = if make { Want::NewDir } else { Want::Dir };
        return state_path(lane, plugin, rel, plugin_states, want).map(Folder::Path);
    }
    plugin_dir(plugin, std::path::Path::new(rel), plugin_states, make).map(Folder::Held)
}

/// A file a caller names: the operator's path, or a plugin's opened below its state folder.
enum Handed {
    Path(std::path::PathBuf),
    Open(std::fs::File),
}

fn handed_file(
    lane: AuthLane,
    plugin: Option<&str>,
    rel: &str,
    plugin_states: &std::path::Path,
) -> Result<Handed, Refusal> {
    if lane.is_operator() {
        return state_path(lane, plugin, rel, plugin_states, Want::File).map(Handed::Path);
    }
    let rel = std::path::Path::new(rel);
    let dir = plugin_dir(plugin, rel.parent().unwrap_or(rel), plugin_states, false)?;
    let name = rel.file_name().ok_or(MISSING)?;
    dir.read(name).map(Handed::Open).map_err(refusal)
}

/// A handover's I/O failure, as hermir's own.
fn moved(op: &'static str, path: &std::path::Path, source: std::io::Error) -> hermir::Error {
    hermir::Error::Io {
        op,
        path: path.to_path_buf(),
        source,
    }
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
    match blocking("emulator_manager", move || {
        crate::emulators::remove(&target, req.purge)
    })
    .await
    {
        Ok(Ok(())) => {
            emit(EventKind::EmulatorsChanged { id });
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(Err(e)) => hermir_err(&e, ("emulator_not_removed", "The emulator wasn't removed")),
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
    match blocking("emulator_manager", crate::emulators::registry).await {
        Ok(Ok(r)) => Json(EmulatorRegistry(r)).into_response(),
        Ok(Err(e)) => hermir_err(
            &e,
            (
                "emulator_registry_not_read",
                "The registry couldn't be read",
            ),
        ),
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
    /// [`SeatQuery::profile`].
    #[serde(default)]
    pub profile: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct SeatQuery {
    /// A profile with a seat of its own: the route answers for that seat's copies.
    #[serde(default)]
    pub profile: Option<String>,
}

/// The seat `profile` plays on, as this host reaches its saves; `None` without a `profile`. A
/// seat host refuses one, since it runs no plugins, and a profile with no seat here is 404.
async fn seat_saves(
    st: &MgmtState,
    profile: Option<String>,
) -> Result<Option<crate::emulators::exchange::Seat>, Response> {
    let Some(profile) = profile else {
        return Ok(None);
    };
    if pf_paths::seat::box_library_dir().is_some() {
        return Err(api_error(
            StatusCode::CONFLICT,
            "A seat host answers for its own saves; ask the host with the plugins",
        ));
    }
    let seat = st.app.profiles.get().and_then(|profiles| {
        profiles
            .list()
            .into_iter()
            .find(|p| p.id == profile)
            .and_then(|p| match p.os_account {
                crate::profiles::OsAccount::Seat { seat } => seat,
                _ => None,
            })
    });
    let no_seat = || {
        api_error(
            StatusCode::NOT_FOUND,
            "That profile has no seat on this box",
        )
    };
    let Some(seat) = seat else {
        return Err(no_seat());
    };
    match blocking("seats", move || crate::emulators::exchange::seat(&seat)).await? {
        Some(seat) => Ok(Some(seat)),
        None => Err(no_seat()),
    }
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
/// the good dumps the catalog knows. `null` when the platform needs none. With `profile`, that
/// seat's copy; a Linux seat answers once its host has checked it at a launch.
#[utoipa::path(
    get,
    path = "/emulators/{id}/firmware",
    tag = "emulators",
    operation_id = "getEmulatorFirmware",
    params(
        ("id" = String, Path, description = "The catalog id"),
        ("platform" = String, Query, description = "A catalog platform id or alias, like `ps2`"),
        ("profile" = Option<String>, Query, description = "A profile with a seat of its own: that seat's copies, on the host with the plugins"),
    ),
    responses(
        (status = OK, description = "The platform's firmware, or null", body = Option<FirmwareStatusView>),
        (status = BAD_REQUEST, description = "A malformed platform, or no copy on this host", body = ApiError),
        (status = NOT_FOUND, description = "Not in the catalog, a profile with no seat, or a seat that hasn't checked it yet", body = ApiError),
        (status = CONFLICT, description = "`profile` on a seat host", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn get_emulator_firmware(
    State(st): State<Arc<MgmtState>>,
    Path(id): Path<String>,
    Query(q): Query<PlatformQuery>,
) -> Response {
    if !valid_platform(&q.platform) {
        return api_error(StatusCode::BAD_REQUEST, "That isn't a platform id");
    }
    let seat = match seat_saves(&st, q.profile).await {
        Ok(seat) => seat,
        Err(r) => return r,
    };
    let checked = blocking("emulator_manager", move || match &seat {
        None => Some(crate::emulators::firmware_status(None, &id, &q.platform)),
        Some(seat) => crate::emulators::exchange::firmware_status(seat, &id, &q.platform),
    })
    .await;
    match checked {
        Ok(Some(Ok(status))) => Json(status.map(firmware_view)).into_response(),
        Ok(Some(Err(e))) => hermir_err(
            &e,
            (
                "emulator_firmware_not_checked",
                "The firmware couldn't be checked",
            ),
        ),
        Ok(None) => api_error(
            StatusCode::NOT_FOUND,
            "That seat hasn't checked this platform's firmware yet",
        ),
        Err(r) => r,
    }
}

fn firmware_view(s: hermir::FirmwareStatus) -> FirmwareStatusView {
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

/// A game folder the caller names: the operator's as named; a plugin's in a root its runner
/// binds (`PluginManifest::reaches`), noting whether a root it may write holds it.
fn game_dir(
    lane: AuthLane,
    plugin: Option<&str>,
    game: Option<&str>,
) -> Result<Option<Game>, Refusal> {
    let Some(game) = game else { return Ok(None) };
    let path = std::path::PathBuf::from(game);
    if path.is_absolute() && lane.is_operator() {
        return Ok(Some(Game::Path(path)));
    }
    let reached = (plugin.and_then(crate::plugins::manifest::for_provider))
        .and_then(|m| Some((m.reaches(&path, false)?, m.reaches(&path, true).is_some())));
    match reached {
        Some(((root, real), writable)) => Ok(Some(Game::Held(HeldGame {
            root,
            real,
            spelled: path,
            writable,
        }))),
        None => Err((
            StatusCode::FORBIDDEN,
            "The game folder is outside the folders this plugin may reach",
        )),
    }
}

/// A game folder a caller names.
enum Game {
    /// The operator's: hermir works on it as named.
    Path(std::path::PathBuf),
    /// A plugin's: hermir works on copies of its units ([`HeldGame`]).
    Held(HeldGame),
}

impl Game {
    /// The folder hermir lists.
    fn listed(&self) -> &std::path::Path {
        match self {
            Game::Path(p) => p,
            Game::Held(h) => &h.real,
        }
    }
}

/// A plugin's game folder: `real` below `root`, the outermost root that holds it, both
/// canonical. The plugin may rewrite it while the host works, so hermir gets a
/// [`handover::Scratch`] holding copies of its units, and each copy moves through a
/// [`handover::Dir`] opened beneath `root`: a link the plugin plants there reaches nothing.
struct HeldGame {
    root: std::path::PathBuf,
    real: std::path::PathBuf,
    spelled: std::path::PathBuf,
    /// A root the plugin may write holds it, so a save may land in it.
    writable: bool,
}

impl HeldGame {
    /// The folder `rel` below the game folder, held open beneath the root.
    fn dir(&self, rel: &std::path::Path, make: bool) -> std::io::Result<handover::Dir> {
        let below = (self.real.strip_prefix(&self.root))
            .map_err(|_| std::io::Error::from(std::io::ErrorKind::PermissionDenied))?;
        handover::Dir::open(&self.root, &below.join(rel), make)
    }

    /// A copy in `into` of each of `units` that lies in the game folder, at its place there.
    fn copy_in<'a>(
        &self,
        units: impl IntoIterator<Item = &'a hermir::SaveUnit>,
        into: &handover::Scratch,
    ) -> std::io::Result<()> {
        for unit in units {
            let Ok(rel) = unit.path.strip_prefix(&self.real) else {
                continue;
            };
            let (Some(parent), Some(name)) = (rel.parent(), rel.file_name()) else {
                continue;
            };
            let to = into.path().join(rel);
            if to.exists() {
                continue; // one file listed under two kinds
            }
            let mut from = self.dir(parent, false)?.read(name)?;
            std::fs::create_dir_all(into.path().join(parent))?;
            std::io::copy(&mut from, &mut std::fs::File::create_new(&to)?)?;
        }
        Ok(())
    }

    /// The file hermir wrote at `written` in `scratch`, put at the same place in the game
    /// folder: where it landed, as the plugin spells it. `None` for a file outside `scratch`,
    /// one in the emulator's own folder. Refused in a game folder the plugin may only read.
    fn put_back(
        &self,
        scratch: &handover::Scratch,
        written: &std::path::Path,
    ) -> std::io::Result<Option<std::path::PathBuf>> {
        let Ok(rel) = written.strip_prefix(scratch.path()) else {
            return Ok(None);
        };
        if !self.writable {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "the plugin may only read this game folder",
            ));
        }
        let parent = rel.parent().unwrap_or(std::path::Path::new(""));
        handover::place(written, &self.dir(parent, true)?)?;
        Ok(Some(self.spelled.join(rel)))
    }
}

/// The game folder hermir exports from or imports into: the operator's as named; for a plugin's,
/// a scratch folder holding a copy of each unit `listed` there that `asked` names.
fn hermir_game(
    game: Option<&Game>,
    listed: impl FnOnce(&std::path::Path) -> hermir::Result<Vec<hermir::SaveUnit>>,
    asked: impl Fn(&hermir::SaveUnit) -> bool,
) -> hermir::Result<(Option<std::path::PathBuf>, Option<handover::Scratch>)> {
    let held = match game {
        None => return Ok((None, None)),
        Some(Game::Path(p)) => return Ok((Some(p.clone()), None)),
        Some(Game::Held(held)) => held,
    };
    let copy =
        handover::Scratch::new().map_err(|e| moved("create", &handover::scratch_base(), e))?;
    let units = listed(&held.real)?;
    (held.copy_in(units.iter().filter(|u| asked(u)), &copy))
        .map_err(|e| moved("copy", &held.spelled, e))?;
    Ok((Some(copy.path().to_path_buf()), Some(copy)))
}

/// The units hermir lists on this host's best copy, or on `seat`'s.
fn list_units(
    seat: Option<&crate::emulators::exchange::Seat>,
    id: &str,
    platform: &str,
    game: &std::path::Path,
) -> hermir::Result<Vec<hermir::SaveUnit>> {
    match seat {
        None => crate::emulators::units(None, id, platform, Some(game)),
        Some(seat) => crate::emulators::exchange::units(seat, id, platform, Some(game)),
    }
}

fn plugin_states() -> std::path::PathBuf {
    pf_paths::config_dir().join("plugin-state")
}

/// List a platform's saves
///
/// The save units of the platform on the best copy of the emulator: name, kind, size and a
/// stamp that changes with the save. A plugin needs the operator's save grant. With `profile`,
/// that seat's units; a Linux seat's are what its host exported at its last launch or exit. A
/// Windows seat host lists them for the operator alone.
#[utoipa::path(
    get,
    path = "/emulators/{id}/saves",
    tag = "emulators",
    operation_id = "getEmulatorSaves",
    params(
        ("id" = String, Path, description = "The catalog id"),
        ("platform" = String, Query, description = "A catalog platform id or alias, like `ps2`"),
        ("game" = Option<String>, Query, description = "The game's folder, for an emulator that keeps saves beside its games"),
        ("profile" = Option<String>, Query, description = "A profile with a seat of its own: that seat's copies, on the host with the plugins"),
    ),
    responses(
        (status = OK, description = "The save units", body = [SaveUnitView]),
        (status = BAD_REQUEST, description = "A malformed platform, or no copy on this host", body = ApiError),
        (status = FORBIDDEN, description = "No save grant, a game folder outside the plugin's reach, or a Windows seat host's plugin asks", body = ApiError),
        (status = NOT_FOUND, description = "Not in the catalog, or a profile with no seat", body = ApiError),
        (status = CONFLICT, description = "`profile` on a seat host", body = ApiError),
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
    if let Some(refused) = seat_write_refusal(lane, crate::plugins::system_seat_host()) {
        return refused;
    }
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
    let seat = match seat_saves(&st, q.profile).await {
        Ok(seat) => seat,
        Err(r) => return r,
    };
    let game = game
        .as_ref()
        .map(Game::listed)
        .map(std::path::Path::to_path_buf);
    match blocking("emulator_manager", move || match &seat {
        None => crate::emulators::units(None, &id, &q.platform, game.as_deref()),
        Some(seat) => crate::emulators::exchange::units(seat, &id, &q.platform, game.as_deref()),
    })
    .await
    {
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
        Ok(Err(e)) => hermir_err(
            &e,
            ("emulator_saves_not_listed", "The saves couldn't be listed"),
        ),
        Err(r) => r,
    }
}

/// Copy saves out
///
/// Writes each named unit into `dir`: the save file, or a tar of a save folder with nothing in
/// its headers that differs between machines. A plugin's `dir` is inside its own state folder.
/// With `profile`, that seat's units; a Linux seat's files are its host's last export, and a
/// unit it didn't export is left out. A Windows seat host takes it from the operator alone.
#[utoipa::path(
    post,
    path = "/emulators/{id}/saves/export",
    tag = "emulators",
    operation_id = "exportEmulatorSaves",
    params(("id" = String, Path, description = "The catalog id"),
        ("profile" = Option<String>, Query, description = "A profile with a seat of its own: that seat's copies, on the host with the plugins"),
    ),
    request_body = ExportUnitsRequest,
    responses(
        (status = OK, description = "The files written", body = [ExportedUnitView]),
        (status = BAD_REQUEST, description = "A malformed platform or unit, or no copy on this host", body = ApiError),
        (status = FORBIDDEN, description = "No save grant, a folder outside the plugin's reach, or a Windows seat host's plugin asks", body = ApiError),
        (status = NOT_FOUND, description = "Not in the catalog, or a profile with no seat", body = ApiError),
        (status = CONFLICT, description = "`profile` on a seat host", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn export_emulator_saves(
    State(st): State<Arc<MgmtState>>,
    Path(id): Path<String>,
    Extension(lane): Extension<AuthLane>,
    who: Option<Extension<PluginIdentity>>,
    Query(q): Query<SeatQuery>,
    ApiJson(req): ApiJson<ExportUnitsRequest>,
) -> Response {
    if let Some(refused) = seat_write_refusal(lane, crate::plugins::system_seat_host()) {
        return refused;
    }
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
    let seat = match seat_saves(&st, q.profile).await {
        Ok(seat) => seat,
        Err(r) => return r,
    };
    let out = match folder(lane, plugin.as_deref(), &req.dir, &plugin_states(), true) {
        Ok(f) => f,
        Err((status, why)) => return api_error(status, why),
    };
    let units: Vec<(hermir::SaveKind, String)> =
        req.units.into_iter().map(|u| (u.kind, u.name)).collect();
    let dir = req.dir;
    let exported = blocking("emulator_manager", move || {
        let platform = &req.platform;
        // hermir exports a plugin's units from a copy of them.
        let (game, _copy) = hermir_game(
            game.as_ref(),
            |game| list_units(seat.as_ref(), &id, platform, game),
            |u| {
                units
                    .iter()
                    .any(|(kind, name)| u.kind == *kind && u.name == *name)
            },
        )?;
        let game = game.as_deref();
        let export = |to: &std::path::Path| match &seat {
            None => crate::emulators::export_units(None, &id, platform, game, &units, to),
            Some(seat) => {
                crate::emulators::exchange::export_units(seat, &id, platform, game, &units, to)
            }
        };
        let held = match &out {
            Folder::Path(p) => return export(p),
            Folder::Held(held) => held,
        };
        // hermir writes into the host's own folder; each file then lands in the plugin's.
        let scratch =
            handover::Scratch::new().map_err(|e| moved("create", &handover::scratch_base(), e))?;
        let rows = export(scratch.path())?;
        for (_, unit) in &rows {
            handover::place(&unit.file, held).map_err(|e| moved("write", &unit.file, e))?;
        }
        Ok(rows)
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
        Ok(Err(e)) => hermir_err(
            &e,
            (
                "emulator_saves_not_exported",
                "The saves weren't copied out",
            ),
        ),
        Err(r) => r,
    }
}

/// Put saves back
///
/// Each unit goes where the emulator keeps it, in the first of its kinds with a place for its
/// name; what was there is kept in hermir's save backups. A unit with no place yet is a step
/// that says so. A plugin's files are inside its own state folder. With `profile`, into that
/// seat's copies; a Linux seat's host puts each back before its next game starts, and the step
/// says so. A Windows seat host takes it from the operator alone.
#[utoipa::path(
    post,
    path = "/emulators/{id}/saves/import",
    tag = "emulators",
    operation_id = "importEmulatorSaves",
    params(("id" = String, Path, description = "The catalog id"),
        ("profile" = Option<String>, Query, description = "A profile with a seat of its own: that seat's copies, on the host with the plugins"),
    ),
    request_body = ImportUnitsRequest,
    responses(
        (status = OK, description = "One step per unit", body = [EmulatorStep]),
        (status = BAD_REQUEST, description = "A malformed platform or unit, a missing file, or no copy on this host", body = ApiError),
        (status = FORBIDDEN, description = "No save grant, a file outside the plugin's reach, or a Windows seat host's plugin asks", body = ApiError),
        (status = NOT_FOUND, description = "Not in the catalog, or a profile with no seat", body = ApiError),
        (status = CONFLICT, description = "`profile` on a seat host", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn import_emulator_saves(
    State(st): State<Arc<MgmtState>>,
    Path(id): Path<String>,
    Extension(lane): Extension<AuthLane>,
    who: Option<Extension<PluginIdentity>>,
    Query(q): Query<SeatQuery>,
    ApiJson(req): ApiJson<ImportUnitsRequest>,
) -> Response {
    if let Some(refused) = seat_write_refusal(lane, crate::plugins::system_seat_host()) {
        return refused;
    }
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
    let seat = match seat_saves(&st, q.profile).await {
        Ok(seat) => seat,
        Err(r) => return r,
    };
    // One row per name, its kinds in the order given.
    let mut rows: Vec<(String, Handed, Vec<hermir::SaveKind>)> = Vec::new();
    for u in req.units {
        if let Some(row) = rows.iter_mut().find(|r| r.0 == u.name) {
            row.2.push(u.kind);
            continue;
        }
        match handed_file(lane, plugin.as_deref(), &u.file, &plugin_states()) {
            Ok(file) => rows.push((u.name, file, vec![u.kind])),
            Err((status, why)) => return api_error(status, why),
        }
    }
    let others = req.others;
    let platform = req.platform;
    let imported = blocking("emulator_manager", move || {
        // hermir reads a copy of a plugin's file in the host's own folder.
        let scratch =
            handover::Scratch::new().map_err(|e| moved("create", &handover::scratch_base(), e))?;
        // The units there now, so hermir backs each up before it writes the new one.
        let (dir, copy) = hermir_game(
            game.as_ref(),
            |game| list_units(seat.as_ref(), &id, &platform, game),
            |u| rows.iter().any(|row| row.0 == u.name),
        )?;
        rows.into_iter()
            .enumerate()
            .map(|(n, (name, handed, kinds))| {
                let file = match handed {
                    Handed::Path(p) => p,
                    Handed::Open(mut from) => {
                        let to = scratch.path().join(n.to_string());
                        std::fs::File::create_new(&to)
                            .and_then(|mut f| std::io::copy(&mut from, &mut f))
                            .map_err(|e| moved("copy", &to, e))?;
                        to
                    }
                };
                let dir = dir.as_deref();
                let mut step = match &seat {
                    None => crate::emulators::import_unit(
                        None, &id, &platform, dir, &kinds, &name, &file, &others,
                    ),
                    Some(seat) => crate::emulators::exchange::import_unit(
                        seat, &id, &platform, dir, &kinds, &name, &file, &others,
                    ),
                }?;
                // What hermir wrote in the copy lands in the plugin's game folder.
                if let (Some(Game::Held(held)), Some(copy)) = (&game, &copy) {
                    match held.put_back(copy, &step.target) {
                        Ok(placed) => step.target = placed.unwrap_or(step.target),
                        // This save alone fails, and says why; the others still go back.
                        Err(e) => {
                            let folder = held.spelled.display();
                            step.outcome = hermir::StepOutcome::Failed;
                            step.note = Some(format!("not written into {folder}: {e}"));
                        }
                    }
                }
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
        Ok(Err(e)) => hermir_err(
            &e,
            ("emulator_saves_not_imported", "The saves weren't put back"),
        ),
        Err(r) => r,
    }
}

/// Install a game's update or DLC
///
/// Hands the files to the best copy of the emulator the way it takes them: a folder it scans,
/// its own installer, or a registration file. One step per file. An emulator with no way says
/// why. A Windows seat host takes it from the operator alone.
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
        (status = FORBIDDEN, description = "A file outside the plugin's reach, or a Windows seat host's plugin asks", body = ApiError),
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
    if let Some(refused) = seat_write_refusal(lane, crate::plugins::system_seat_host()) {
        return refused;
    }
    if !valid_platform(&req.platform) {
        return api_error(StatusCode::BAD_REQUEST, "That isn't a platform id");
    }
    if !matches!(req.kind.as_str(), "update" | "dlc") {
        return api_error(StatusCode::BAD_REQUEST, "The kind is `update` or `dlc`");
    }
    let plugin = who.map(|Extension(w)| w.0);
    let named = req.files.clone();
    let (cid, cplatform, ckind) = (id.clone(), req.platform.clone(), req.kind.clone());
    let resolved = blocking("emulator_manager", move || {
        let manifest = plugin
            .as_deref()
            .and_then(crate::plugins::manifest::for_provider);
        // An emulator that copies the add-on in reads a mirror that goes after the install.
        // One that keeps the path it is given gets the file where it lies, and nothing is copied.
        let keeps = crate::emulators::content_keeps_path(&cid, &cplatform, &ckind);
        let scratch = match lane.is_operator() || keeps {
            true => None,
            false => Some(handover::Scratch::new().map_err(refusal)?),
        };
        let copy = scratch.as_ref().map(handover::Scratch::path);
        let mut held = Vec::new();
        let files = named
            .iter()
            .map(|f| {
                let path = std::path::Path::new(f);
                if !lane.is_operator() {
                    let (id, states) = (plugin.as_deref(), plugin_states());
                    plugin_content(id, manifest.as_ref(), path, &states, copy, &mut held)
                } else if path.is_absolute() {
                    Ok(path.to_path_buf())
                } else {
                    Err(MISSING)
                }
            })
            .collect::<Result<Vec<_>, Refusal>>()?;
        Ok((files, (scratch, held)))
    })
    .await;
    let (files, scratch) = match resolved {
        Ok(Ok(files)) => files,
        Ok(Err((status, why))) => return api_error(status, why),
        Err(r) => return r,
    };
    // A step names the file as the caller did, never the host's mirror of it.
    let spelled: Vec<(std::path::PathBuf, String)> = files
        .iter()
        .cloned()
        .zip(req.files.iter().cloned())
        .collect();
    let installed = blocking("emulator_manager", move || {
        let installed = crate::emulators::install_content(&id, &req.platform, &req.kind, &files);
        drop(scratch);
        installed
    })
    .await;
    match installed {
        Ok(Ok(steps)) => Json(
            steps
                .into_iter()
                .map(|s| EmulatorStep {
                    name: None,
                    source: Some(spelled_source(&spelled, &s.source)),
                    target: s.target.to_string_lossy().into_owned(),
                    outcome: wire(s.outcome),
                    note: s.note,
                })
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Ok(Err(e)) => hermir_err(
            &e,
            (
                "emulator_addons_not_installed",
                "The add-ons weren't installed",
            ),
        ),
        Err(r) => r,
    }
}

/// `source`, a path hermir reports on, as the caller spelled it: a file it handed over, or a
/// folder above one (a title folder), spelled as many levels above the caller's own file. A
/// path of neither kind is given as it is.
fn spelled_source(spelled: &[(std::path::PathBuf, String)], source: &std::path::Path) -> String {
    let caller = spelled.iter().find_map(|(at, file)| {
        let up = at.strip_prefix(source).ok()?.components().count();
        std::path::Path::new(file).ancestors().nth(up)
    });
    caller.unwrap_or(source).to_string_lossy().into_owned()
}

/// Adopt a copy of an emulator
///
/// Points the host at a copy of the caller's own, one no rule finds (a portable build unpacked
/// anywhere), so it is listed and launched like any other; `forget` drops it again. The
/// operator's lane, and a seat's person on their seat host. On a person's own account only that
/// person adopts one: anyone else's copy would run as them.
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
        (status = FORBIDDEN, description = "A seat host on a person's own account, from any lane but that person's", body = ApiError),
    )
)]
pub(crate) async fn adopt_emulator(
    Path(id): Path<String>,
    Extension(lane): Extension<AuthLane>,
    ApiJson(req): ApiJson<AdoptRequest>,
) -> Response {
    if pf_paths::seat::own_account() && !req.forget && lane != AuthLane::Person {
        return api_error_coded(
            StatusCode::FORBIDDEN,
            "emulator_adopt_own_account",
            "An emulator copy can't be added by hand on a profile that uses an existing account, \
             because it would run as that person.",
        );
    }
    let target = id.clone();
    let adopted = blocking("emulator_manager", move || {
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
        Ok(Err(e)) => hermir_err(&e, ("emulator_copy_not_adopted", "The copy wasn't adopted")),
        Err(r) => r,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A Windows seat host's plugin hands it no emulator file, and the refusal says why; the
    /// operator and every other host stay as they were.
    #[test]
    fn a_windows_seat_host_takes_emulator_writes_from_the_operator_alone() {
        let refused = seat_write_refusal(AuthLane::Plugin, true).expect("the plugin is refused");
        assert_eq!(refused.status(), StatusCode::FORBIDDEN);
        assert!(seat_write_refusal(AuthLane::Person, true).is_some());
        assert!(seat_write_refusal(AuthLane::Admin, true).is_none());
        assert!(seat_write_refusal(AuthLane::Plugin, false).is_none());
    }

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

    /// What the host opens for a plugin is held open below its state folder: a folder or file
    /// through a link is refused, and nothing lands outside.
    #[test]
    fn a_plugin_s_files_open_only_below_its_own_state_folder() {
        let states = tempfile::tempdir().unwrap();
        let own = states.path().join("rom-manager");
        std::fs::create_dir_all(own.join("firmware/ps2")).unwrap();
        std::fs::write(own.join("save.srm"), b"x").unwrap();
        let lane = AuthLane::Plugin;
        let dir = |rel: &str, make| {
            folder(lane, Some("rom-manager"), rel, states.path(), make).map_err(|(s, _)| s)
        };
        let file = |rel: &str| {
            handed_file(lane, Some("rom-manager"), rel, states.path()).map_err(|(s, _)| s)
        };
        assert!(matches!(dir("firmware/ps2", false), Ok(Folder::Held(_))));
        assert!(matches!(dir("saves/out", true), Ok(Folder::Held(_))));
        assert!(own.join("saves/out").is_dir());
        assert_eq!(
            dir("firmware/none", false).err(),
            Some(StatusCode::BAD_REQUEST)
        );
        assert_eq!(dir("../escape", true).err(), Some(StatusCode::FORBIDDEN));
        assert!(matches!(file("save.srm"), Ok(Handed::Open(_))));
        assert_eq!(file("firmware").err(), Some(StatusCode::BAD_REQUEST));
        assert_eq!(file("/etc/passwd").err(), Some(StatusCode::FORBIDDEN));
        #[cfg(unix)]
        {
            let elsewhere = tempfile::tempdir().unwrap();
            std::fs::write(elsewhere.path().join("token"), b"secret").unwrap();
            std::os::unix::fs::symlink(elsewhere.path(), own.join("link")).unwrap();
            std::os::unix::fs::symlink(elsewhere.path().join("token"), own.join("t")).unwrap();
            assert_eq!(dir("link/new", true).err(), Some(StatusCode::FORBIDDEN));
            assert!(!elsewhere.path().join("new").exists());
            assert_eq!(file("t").err(), Some(StatusCode::FORBIDDEN));
            assert_eq!(file("link/token").err(), Some(StatusCode::FORBIDDEN));
        }
    }

    /// A step names what the plugin handed over, never the host's copy of it: the file, or the
    /// title folder as many levels above it.
    #[test]
    fn a_step_s_source_is_the_caller_s_spelling() {
        use std::path::{Path, PathBuf};
        let copy = PathBuf::from("/host/copy/roots/ab/wiiu/Title/code/app.rpx");
        let spelled = [(copy.clone(), "/games/wiiu/Title/code/app.rpx".to_string())];
        let named = |source: &str| spelled_source(&spelled, Path::new(source));
        assert_eq!(
            named(copy.to_str().unwrap()),
            "/games/wiiu/Title/code/app.rpx"
        );
        assert_eq!(named("/host/copy/roots/ab/wiiu/Title"), "/games/wiiu/Title");
        assert_eq!(named("/elsewhere/file"), "/elsewhere/file");
    }

    /// A state folder the person moved elsewhere by a link is followed to where it lies.
    #[cfg(unix)]
    #[test]
    fn a_linked_state_folder_is_followed() {
        let tmp = tempfile::tempdir().unwrap();
        let states = tmp.path().join("plugin-state");
        std::fs::create_dir_all(states.join("real")).unwrap();
        std::os::unix::fs::symlink(states.join("real"), states.join("linked")).unwrap();
        let rel = std::path::Path::new("x");
        let real = states.join("real").canonicalize().unwrap();
        assert_eq!(own_state(Some("real"), &states, rel), Ok(real.clone()));
        assert_eq!(own_state(Some("linked"), &states, rel), Ok(real));
        assert_eq!(own_state(Some("gone"), &states, rel), Err(NO_OWN_TOKEN));
    }

    /// hermir works on copies of a plugin's game folder: a link planted there is neither read
    /// nor written through, and a save hermir wrote lands beside the game, as the plugin spells
    /// it.
    #[cfg(unix)]
    #[test]
    fn a_plugin_s_game_folder_moves_only_beneath_its_root() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().unwrap();
        let tmp = tmp.path().canonicalize().unwrap();
        let (root, outside) = (tmp.join("roms"), tmp.join("outside"));
        let game = root.join("gba");
        std::fs::create_dir_all(&game).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("hooks.json"), b"theirs").unwrap();
        std::fs::write(game.join("Emerald.sav"), b"old").unwrap();
        symlink(outside.join("hooks.json"), game.join("Ruby.sav")).unwrap();
        symlink(&outside, game.join("sub")).unwrap();
        let held = HeldGame {
            root: root.clone(),
            real: game.clone(),
            spelled: game.clone(),
            writable: true,
        };
        let unit = |rel: &str| hermir::SaveUnit {
            kind: hermir::SaveKind::Save,
            name: rel.replace('/', "__"),
            shared: false,
            written_at_start: false,
            size: 0,
            files: 1,
            modified: 0,
            path: game.join(rel),
        };
        let copy = handover::Scratch::new_in(&tmp.join("scratch")).unwrap();
        for linked in ["Ruby.sav", "sub/hooks.json"] {
            assert!(held.copy_in([&unit(linked)], &copy).is_err(), "{linked}");
        }
        held.copy_in([&unit("Emerald.sav")], &copy).unwrap();
        assert_eq!(
            std::fs::read(copy.path().join("Emerald.sav")).unwrap(),
            b"old"
        );

        let back = |rel: &str| {
            let written = copy.path().join(rel);
            std::fs::create_dir_all(written.parent().unwrap()).unwrap();
            std::fs::write(&written, b"new").unwrap();
            held.put_back(&copy, &written)
        };
        for rel in ["Emerald.sav", "Ruby.sav", "new/y.sav"] {
            assert_eq!(back(rel).unwrap(), Some(game.join(rel)));
            assert_eq!(std::fs::read(game.join(rel)).unwrap(), b"new", "{rel}");
        }
        assert!(back("sub/x.sav").is_err());
        assert_eq!(
            std::fs::read(outside.join("hooks.json")).unwrap(),
            b"theirs"
        );
        assert!(!outside.join("x.sav").exists());
        let skipped = held.put_back(&copy, std::path::Path::new("Emerald.sav"));
        assert_eq!(skipped.unwrap(), None);
        // A folder the plugin may only read takes no save; one in the emulator's folder goes.
        let read_only = HeldGame {
            writable: false,
            ..held
        };
        assert!(read_only
            .put_back(&copy, &copy.path().join("Emerald.sav"))
            .is_err());
        let elsewhere = read_only.put_back(&copy, std::path::Path::new("Emerald.sav"));
        assert_eq!(elsewhere.unwrap(), None);
    }
}
