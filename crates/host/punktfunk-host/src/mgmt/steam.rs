//! `/steam/entries`: what the Decky plugin may sync into the box's own Steam as entries of their
//! own, and the map of what it synced ([`crate::library::SteamEntries`]). Admin lane only: the
//! plugin calls through `punktfunk-host ctl steam`, which reads the host's token itself.

use super::shared::*;
use crate::library::{ArtKind, GameEntry, GameRole, SteamEntries};
use std::collections::BTreeMap;

/// The titles a Steam entry may start, and where they come from.
#[derive(Serialize, ToSchema)]
pub(crate) struct SteamEntryList {
    /// Steam owns the box's screen (Game Mode): only what Steam starts is shown there.
    steam_owns_desk: bool,
    sources: Vec<SteamSource>,
    entries: Vec<SteamEntry>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct SteamSource {
    /// The source's id, as each entry's `source` names it.
    #[schema(example = "rom-manager")]
    id: String,
    #[schema(example = "ROM Manager")]
    name: String,
    /// Its titles in `entries`.
    games: usize,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct SteamEntry {
    id: String,
    title: String,
    source: String,
    /// The platform as the library has it.
    #[serde(skip_serializing_if = "Option::is_none")]
    platform: Option<String>,
    /// The art the host has for it: `portrait`, `hero`, `logo`, `header`.
    art: Vec<&'static str>,
}

/// List what may be synced into Steam
///
/// The titles of the owner profile's library that a Steam entry of their own may start: games
/// with a launch recipe, no launchers and no Steam titles. Each names its source, its platform
/// as the library has it and the art kinds the host serves for it at `GET /library/art`. Admin
/// lane only.
#[utoipa::path(
    get,
    path = "/steam/entries",
    tag = "library",
    operation_id = "listSteamEntries",
    responses(
        (status = OK, description = "The titles and their sources", body = SteamEntryList),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn list_steam_entries(State(st): State<Arc<MgmtState>>) -> Response {
    let (view, _) = super::session::desk_library(&st);
    let listed = tokio::task::spawn_blocking(move || {
        #[cfg(target_os = "linux")]
        let steam_owns_desk = crate::vdisplay::steam_owns_desk();
        #[cfg(not(target_os = "linux"))]
        let steam_owns_desk = false;
        let games = crate::library::all_games_for(&view);
        list(games.iter().filter(|g| syncable(g)), steam_owns_desk)
    })
    .await;
    match listed {
        Ok(list) => Json(list).into_response(),
        Err(e) => host_error("steam entry list did not finish", &e),
    }
}

/// Store the map of synced entries
///
/// The plugin sends the whole map after each sync: version 1, each synced title's library id →
/// the 32-bit app id of its entry, and `own`, every other entry it made. The library leaves out
/// the `steam:<app id>` title of each, and a launch of a synced title in Game Mode asks Steam to
/// run its entry. Admin lane only.
#[utoipa::path(
    put,
    path = "/steam/entries",
    tag = "library",
    operation_id = "setSteamEntries",
    request_body = SteamEntries,
    responses(
        (status = NO_CONTENT, description = "Stored"),
        (status = BAD_REQUEST, description = "Not a version 1 map, a Steam title or an app id of 0 in it, two titles on one app id, or too many entries", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = INTERNAL_SERVER_ERROR, description = "The map was not saved", body = ApiError),
    )
)]
pub(crate) async fn set_steam_entries(ApiJson(map): ApiJson<SteamEntries>) -> Response {
    if let Err(why) = map.check() {
        return api_error(StatusCode::BAD_REQUEST, why);
    }
    match crate::library::save_steam_entries(&map) {
        Ok(changed) => {
            if changed {
                tracing::info!(
                    synced = map.entries.len(),
                    own = map.own.len(),
                    "management API: Steam entries stored"
                );
                let source = "steam".to_string();
                crate::events::emit(crate::events::EventKind::LibraryChanged { source });
            }
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) => host_error("store the Steam entries", &format!("{e:#}")),
    }
}

/// A title a Steam entry of its own may start: one with a recipe, no launcher (the host's Game
/// Mode tile is one), no seat's own title, and no title Steam starts itself, whichever source
/// lists it: its entry would be a double of Steam's own.
fn syncable(g: &GameEntry) -> bool {
    let recipe = g.launch.as_ref().is_some_and(|l| l.kind != "steam_appid");
    recipe && g.role != GameRole::Launcher && !g.own && !g.id.starts_with("steam:")
}

/// Where a title comes from: its claimed store, else the plugin that lists it, else `custom`.
fn source_of(g: &GameEntry) -> &str {
    match g.store.as_str() {
        "custom" => g.provider.as_deref().unwrap_or("custom"),
        store => store,
    }
}

/// `games` as the answer lists them, each source once with its count and its name: the live
/// plugin's title, else the console's label.
fn list<'a>(games: impl Iterator<Item = &'a GameEntry>, steam_owns_desk: bool) -> SteamEntryList {
    let mut sources: BTreeMap<String, (Option<String>, usize)> = BTreeMap::new();
    let entries = games
        .map(|g| {
            let source = source_of(g).to_string();
            let seen = sources.entry(source.clone()).or_insert((None, 0));
            seen.0 = seen.0.take().or_else(|| g.provider.clone());
            seen.1 += 1;
            let art = ArtKind::ALL.into_iter();
            let art = art.filter(|k| crate::library::art_field(&g.art, *k).is_some());
            SteamEntry {
                id: g.id.clone(),
                title: g.title.clone(),
                source,
                platform: g.meta.platform.clone(),
                art: art.map(ArtKind::name).collect(),
            }
        })
        .collect();
    let registry = crate::plugin_host::registry::registry();
    let sources = sources
        .into_iter()
        .map(|(id, (provider, games))| SteamSource {
            name: provider
                .and_then(|p| registry.title(&p))
                .unwrap_or_else(|| crate::library::store_label(&id)),
            id,
            games,
        })
        .collect();
    SteamEntryList {
        steam_owns_desk,
        sources,
        entries,
    }
}
