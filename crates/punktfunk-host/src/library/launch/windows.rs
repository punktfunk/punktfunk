//! Windows launch recipes: [`launch_title`] spawns a library id as the signed-in user of the
//! host's WTS session ([`crate::interactive::spawn_as_current_session_user`]); every other
//! item maps a [`LaunchSpec`] kind to the argv that spawn gets, or finds the store exe it needs.

use super::*;

/// Each activation below was checked on a real install; an unverified one ships a dead tile.
pub(super) const LAUNCHER_UI_STORES: &[&str] =
    &["playnite", "epic", "gog", "xbox", "hydra-big-picture"];

/// The Xbox app's package identity and app id (its `AppxManifest.xml`). The publisher hash
/// is read from AppRepository at launch, like the `xbox` kind.
const XBOX_APP_IDENTITY: &str = "Microsoft.GamingApp";
const XBOX_APP_ID: &str = "Microsoft.Xbox.App";

/// Recipe is resolved in [`launch_title`]; a missing one still yields a target here.
pub(super) fn launch_target(
    entry: GameEntry,
    game: crate::gamelease::GameRef,
) -> Option<LaunchTarget> {
    Some(LaunchTarget {
        game,
        launcher: entry.role == GameRole::Launcher,
        detect: entry.detect,
        command: None,
        own_workspace: entry.on_window.own_workspace(),
        on_window: entry.on_window,
    })
}

/// The async handshake never resolves a Windows launch ahead of time: `launch_title` does
/// it on the stream thread, and a miss there is an ordinary "not launched".
pub fn launch_is_resolvable(_id: &str) -> bool {
    false
}

/// Can this box open a known `launcher_ui` value now? Yes once its exe or package is found.
pub(super) fn launcher_ui_installed(value: &str) -> bool {
    match value {
        "playnite" => playnite_fullscreen_exe().is_some(),
        "epic" => epic_launcher_exe().is_some(),
        "gog" => galaxy_exe().is_some(),
        "xbox" => xbox_pfn(XBOX_APP_IDENTITY).is_some(),
        "hydra-big-picture" => hydra_exe().is_some(),
        _ => false,
    }
}

/// Command line, working dir, and whether the started process **is** the game.
///
/// Almost every Windows recipe is a protocol hand-off (`steam://`, `playnite://`)
/// whose pid is a forwarder's: already-running launcher → pid dies while the
/// game still loads; cold launcher → pid becomes the launcher and never exits.
/// Only a direct game (or operator) start goes on
/// [`crate::gamelease::LeaseRequest::spawned`]; a hand-off pid is dropped and
/// the lease uses detect signals.
pub struct WinRecipe {
    pub cmdline: String,
    pub workdir: Option<std::path::PathBuf>,
    /// `false` for a protocol/launcher hand-off (see type docs).
    pub owns_game: bool,
}

impl WinRecipe {
    /// A launcher exe started in its own dir. It forwards to a running instance and exits.
    fn handoff_exe(exe: std::path::PathBuf) -> Self {
        Self {
            cmdline: format!("\"{}\"", exe.display()),
            workdir: exe.parent().map(std::path::Path::to_path_buf),
            owns_game: false,
        }
    }

    fn handoff(cmdline: String) -> Self {
        Self {
            cmdline,
            workdir: None,
            owns_game: false,
        }
    }

    fn game(cmdline: String, workdir: Option<std::path::PathBuf>) -> Self {
        Self {
            cmdline,
            workdir,
            owns_game: true,
        }
    }
}

/// Pid from a Windows launch, plus whether it is the game's ([`WinRecipe::owns_game`]).
pub struct WindowsLaunch {
    pub pid: u32,
    /// Whether this pid is the game, not a protocol forwarder.
    pub owns_game: bool,
}

impl WindowsLaunch {
    /// Lease pid: `None` when the host only started a hand-off.
    pub fn tracked_pid(&self) -> Option<u32> {
        self.owns_game.then_some(self.pid)
    }
}

/// Launches a store-qualified library id as the user of the host's WTS session.
///
/// [`windows_launch_for`] maps the recipe and
/// [`crate::interactive::spawn_as_current_session_user`] starts it after capture
/// is live. The returned PID enters
/// [`crate::gamelease::LeaseRequest::spawned`] only when it owns the game rather
/// than a protocol hand-off.
pub fn launch_title(id: &str) -> Result<WindowsLaunch> {
    let entry = all_games()
        .into_iter()
        .find(|g| g.id == id)
        .filter(|g| g.launch.is_some())
        .ok_or_else(|| anyhow::anyhow!("no launchable library entry '{id}'"))?;
    let spec = entry.launch.clone().expect("filtered to Some above");
    // An `exec` entry is built from the publishing plugin's manifest; `windows_launch_for` has
    // no `exec` arm, so an entry that fails validation falls through to "no recipe" below.
    let recipe = exec_recipe(&entry)
        .map(|r| WinRecipe::game(r.command_line(), r.cwd))
        .or_else(|| windows_launch_for(&spec))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "library entry '{id}' has no Windows launch recipe (kind '{}')",
                spec.kind
            )
        })?;
    let WinRecipe {
        cmdline,
        workdir,
        owns_game,
    } = recipe;
    let pid = crate::interactive::spawn_as_current_session_user(&cmdline, workdir.as_deref())
        .with_context(|| format!("launch '{id}' as the current WTS session user"))?;
    tracing::info!(
        launch_id = id,
        %cmdline,
        pid,
        owns_game,
        "launched library title as the current WTS session user"
    );
    Ok(WindowsLaunch { pid, owns_game })
}

/// Pure map from [`LaunchSpec`] to a spawn recipe. `None` = no Windows recipe.
///
/// CreateProcessAsUserW does no shell or protocol resolution: URI/flags go to
/// a concrete EXE as argv. `exec` is absent; [`exec_recipe`] builds it from the manifest.
fn windows_launch_for(spec: &LaunchSpec) -> Option<WinRecipe> {
    match spec.kind.as_str() {
        "steam_appid" => {
            if !valid_steam_appid(&spec.value) {
                return None;
            }
            let uri = format!("steam://rungameid/{}", spec.value);
            // Forwarder either way: running Steam posts the URI and exits; cold Steam *is* the client.
            steam_cmdline(&uri).map(WinRecipe::handoff)
        }
        // Steam client UI (design D4). Same Steam as `steam_appid`.
        "steam_ui" => {
            let uri = match spec.value.as_str() {
                "bigpicture" => "steam://open/bigpicture",
                "desktop" => "steam://open/main",
                _ => return None,
            };
            steam_cmdline(uri).map(WinRecipe::handoff)
        }
        // explorer.exe + Epic URI: one argv, no shell. Same pattern as the Steam fallback.
        "epic" => epic_launch_uri(&spec.value)
            .map(|uri| WinRecipe::handoff(format!("explorer.exe \"{uri}\""))),
        // Direct exe spawn (not a Galaxy hand-off). `gog_spawn` re-confines the plugin triple.
        "gog" => gog_spawn(&spec.value).map(|(cmdline, workdir)| WinRecipe::game(cmdline, workdir)),
        // Direct exe spawn, only of a path a signed-in user's Game Bar list names.
        "gamebar" => gamebar_spawn_in(&spec.value, &gamebar_exes()),
        // shell:AppsFolder AUMID. UWP activation fails as SYSTEM/session-0; spawn uses the user token.
        "aumid" => valid_aumid(&spec.value).then(|| {
            WinRecipe::handoff(format!("explorer.exe \"shell:AppsFolder\\{}\"", spec.value))
        }),
        // Plugin sends `<Identity>!<AppId>` from MicrosoftGame.config. AppRepository
        // is LocalSystem-only (LocalService cannot enumerate it), so the host
        // completes the PFN at launch — a cached hash would stale on package update.
        "xbox" => {
            let (identity, app_id) = spec.value.split_once('!')?;
            if !aumid_part(identity) || !aumid_part(app_id) {
                return None;
            }
            let pfn = xbox_pfn(identity)?;
            Some(WinRecipe::handoff(format!(
                "explorer.exe \"shell:AppsFolder\\{pfn}!{app_id}\""
            )))
        }
        // explorer.exe + playnite:// — Playnite maps the id to the owning store.
        // Typed kind: `command` is operator-only, so a plugin cannot publish one.
        "playnite" => valid_playnite_id(&spec.value).then(|| {
            WinRecipe::handoff(format!(
                "explorer.exe \"playnite://playnite/start/{}\"",
                spec.value
            ))
        }),
        // Ubisoft Connect and Amazon Games register a protocol handler and nothing else;
        // explorer.exe resolves it as the user, the `epic` shape. Ids are charset-guarded.
        "uplay" => valid_uplay_id(&spec.value).then(|| {
            WinRecipe::handoff(format!("explorer.exe \"uplay://launch/{}/0\"", spec.value))
        }),
        "amazon" => valid_amazon_id(&spec.value).then(|| {
            WinRecipe::handoff(format!(
                "explorer.exe \"amazon-games://play/{}\"",
                spec.value
            ))
        }),
        // The EA app answers `origin2://` for a game's content id, the `uplay` shape.
        "ea" => valid_ea_id(&spec.value).then(|| {
            WinRecipe::handoff(format!(
                "explorer.exe \"origin2://game/launch/?offerIds={}\"",
                spec.value
            ))
        }),
        // Rockstar's launcher starts a title by its folder. Both paths come from the
        // launcher's uninstall entries, so the plugin names only the title id.
        "rockstar" => {
            if !valid_rockstar_title(&spec.value) {
                return None;
            }
            let (launcher, dir) = rockstar_paths(&spec.value)?;
            Some(WinRecipe::handoff(format!(
                "\"{}\" -launchTitleInFolder \"{}\"",
                launcher.display(),
                dir.display()
            )))
        }
        // `battlenet://<code>` only opens the game's page; `--exec="launch <code>"` on the
        // client's exe starts it. No exe found refuses the launch rather than opening a page.
        "battlenet" => {
            if !valid_battlenet_code(&spec.value) {
                return None;
            }
            let exe = battlenet_exe()?;
            Some(WinRecipe::handoff(format!(
                "\"{}\" --exec=\"launch {}\"",
                exe.display(),
                spec.value
            )))
        }
        // Launcher UIs (design D4). Playnite Fullscreen is spawned directly: `playnite://` opens
        // the desktop app, and the .NET app expects its install dir as workdir. The store
        // clients forward to a running instance, so they are hand-offs. Hydra opens Big
        // Picture from argv only, a running instance included.
        "launcher_ui" => match spec.value.as_str() {
            "playnite" => playnite_fullscreen_exe().map(|exe| {
                let dir = exe.parent().map(std::path::Path::to_path_buf);
                WinRecipe::game(format!("\"{}\"", exe.display()), dir)
            }),
            "epic" => epic_launcher_exe().map(WinRecipe::handoff_exe),
            "gog" => galaxy_exe().map(WinRecipe::handoff_exe),
            "xbox" => xbox_pfn(XBOX_APP_IDENTITY).map(|pfn| {
                WinRecipe::handoff(format!(
                    "explorer.exe \"shell:AppsFolder\\{pfn}!{XBOX_APP_ID}\""
                ))
            }),
            "hydra-big-picture" => hydra_exe()
                .map(|exe| WinRecipe::handoff(format!("\"{}\" --big-picture", exe.display()))),
            _ => None,
        },
        // Operator command. `cmd.exe /c` blocks until it returns, so the pid is that command's life.
        "command" => {
            let v = spec.value.trim();
            (!v.is_empty()).then(|| WinRecipe::game(format!("cmd.exe /c {v}"), None))
        }
        _ => None,
    }
}

/// `steam.exe` with `uri`, else `explorer.exe` for the `steam://` handler in the user's hive. A
/// Windows seat starts only its own copy under its own IPC name ([`pf_seats::steam`]): any other
/// Steam closes the one running in another session. `None` on a seat until that copy exists.
fn steam_cmdline(uri: &str) -> Option<String> {
    if !pf_paths::seat::is_seat_host() {
        return Some(match steam_exe() {
            Some(exe) => format!("\"{}\" \"{uri}\"", exe.display()),
            None => format!("explorer.exe \"{uri}\""),
        });
    }
    let exe = pf_paths::seat::seat_steam().filter(|exe| exe.is_file());
    match (exe, pf_paths::seat::seat_id().ok().flatten()) {
        (Some(exe), Some(id)) => Some(seat_steam_cmdline(&exe, &id, uri)),
        _ => {
            tracing::warn!(
                "this seat has no Steam client of its own yet; its Steam titles don't start"
            );
            None
        }
    }
}

fn seat_steam_cmdline(exe: &std::path::Path, seat_id: &str, uri: &str) -> String {
    format!(
        "\"{}\" -master_ipc_name_override {} \"{uri}\"",
        exe.display(),
        pf_seats::steam::ipc_name(seat_id)
    )
}

/// Default `steam.exe` only. Non-default installs use the explorer.exe protocol
/// fallback. Probes Program Files, `ProgramFiles(x86)` first.
fn steam_exe() -> Option<std::path::PathBuf> {
    for var in ["ProgramFiles(x86)", "ProgramFiles", "ProgramW6432"] {
        if let Some(pf) = std::env::var_os(var) {
            let p = std::path::PathBuf::from(pf).join("Steam").join("steam.exe");
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

/// Exe of a machine-wide protocol handler, `<scheme>\shell\open\command` under HKCR. A
/// per-user registration lives in HKCU, which LocalSystem does not see.
fn handler_exe(scheme: &str) -> Option<std::path::PathBuf> {
    use winreg::enums::{HKEY_CLASSES_ROOT, KEY_READ};
    use winreg::RegKey;

    RegKey::predef(HKEY_CLASSES_ROOT)
        .open_subkey_with_flags(format!(r"{scheme}\shell\open\command"), KEY_READ)
        .and_then(|k| k.get_value::<String, _>(""))
        .ok()
        .and_then(|c| exe_from_shell_command(&c).map(std::path::PathBuf::from))
        .filter(|p| p.is_file())
}

/// The Epic launcher registers `com.epicgames.launcher://` machine-wide.
fn epic_launcher_exe() -> Option<std::path::PathBuf> {
    handler_exe("com.epicgames.launcher")
}

/// GOG Galaxy's exe from its HKLM install paths. Its `goggalaxy://` handler is per-user.
fn galaxy_exe() -> Option<std::path::PathBuf> {
    use winreg::enums::{HKEY_LOCAL_MACHINE, KEY_READ};
    use winreg::RegKey;

    let key = RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey_with_flags(r"SOFTWARE\WOW6432Node\GOG.com\GalaxyClient", KEY_READ)
        .ok()?;
    let dir: String = key
        .open_subkey_with_flags("paths", KEY_READ)
        .and_then(|k| k.get_value("client"))
        .ok()?;
    let exe: String = key
        .get_value("clientExecutable")
        .unwrap_or_else(|_| "GalaxyClient.exe".into());
    Some(std::path::PathBuf::from(dir).join(exe)).filter(|p| p.is_file())
}

/// The Battle.net client's exe: its `battlenet://` handler first (it follows a non-default
/// install), else the default under Program Files (x86).
fn battlenet_exe() -> Option<std::path::PathBuf> {
    if let Some(exe) = handler_exe("battlenet") {
        return Some(exe);
    }
    for var in ["ProgramFiles(x86)", "ProgramFiles"] {
        if let Some(pf) = std::env::var_os(var) {
            let p = std::path::PathBuf::from(pf)
                .join("Battle.net")
                .join("Battle.net.exe");
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

/// The Rockstar Games Launcher's `Launcher.exe` and `title`'s install folder, from the
/// machine-wide uninstall entries the launcher writes. Either missing refuses the launch.
fn rockstar_paths(title: &str) -> Option<(std::path::PathBuf, std::path::PathBuf)> {
    use winreg::enums::{HKEY_LOCAL_MACHINE, KEY_READ};
    use winreg::RegKey;

    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    let (mut launcher, mut folder) = (None, None);
    for path in [
        r"Software\Microsoft\Windows\CurrentVersion\Uninstall",
        r"Software\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall",
    ] {
        let Ok(uninstall) = hklm.open_subkey_with_flags(path, KEY_READ) else {
            continue;
        };
        for name in uninstall.enum_keys().flatten() {
            let Ok(entry) = uninstall.open_subkey_with_flags(&name, KEY_READ) else {
                continue;
            };
            let location = entry
                .get_value::<String, _>("InstallLocation")
                .map(|l| std::path::PathBuf::from(l.trim().trim_matches('"')))
                .unwrap_or_default();
            if location.as_os_str().is_empty() || location.to_string_lossy().contains('"') {
                continue;
            }
            let display: String = entry.get_value("DisplayName").unwrap_or_default();
            let uninstall_cmd: String = entry.get_value("UninstallString").unwrap_or_default();
            if display == "Rockstar Games Launcher" {
                launcher = Some(location.join("Launcher.exe")).filter(|p| p.is_file());
            } else if rockstar_uninstall_title(&uninstall_cmd)
                .is_some_and(|t| t.eq_ignore_ascii_case(title))
                && location.is_dir()
            {
                folder = Some(location);
            }
        }
    }
    Some((launcher?, folder?))
}

/// PackageFamilyName from `AppRepository\Packages\<PackageFullName>`:
/// `Name_PublisherHash`. Read the hash; never compute it.
///
/// LocalSystem can enumerate that dir; the plugin runner (LocalService) cannot.
/// The plugin sends Identity from MicrosoftGame.config; this completes the PFN.
fn xbox_pfn(identity: &str) -> Option<String> {
    let pkgs = std::path::PathBuf::from(std::env::var_os("ProgramData")?)
        .join("Microsoft")
        .join("Windows")
        .join("AppRepository")
        .join("Packages");
    let prefix = format!("{identity}_");
    for e in std::fs::read_dir(&pkgs).ok()?.flatten() {
        let dn = e.file_name().to_string_lossy().into_owned();
        if dn.starts_with(&prefix) {
            if let Some(pfn) = pfn_from_full(&dn, identity) {
                return Some(pfn);
            }
        }
    }
    None
}

/// `Name_Version_Arch_ResourceId_PublisherHash` → `Name_PublisherHash`.
/// Hash is the last `_`-segment; `Name` is the caller's identity.
fn pfn_from_full(dir_name: &str, identity: &str) -> Option<String> {
    let hash = dir_name.rsplit('_').next()?;
    (!hash.is_empty() && hash != dir_name).then(|| format!("{identity}_{hash}"))
}

/// Playnite Fullscreen exe, if found. `playnite://` is registered to the
/// desktop app, so a URI cannot open fullscreen. `None` drops the tile.
fn playnite_fullscreen_exe() -> Option<std::path::PathBuf> {
    const EXE: &str = "Playnite.FullscreenApp.exe";
    playnite_install_dirs()
        .into_iter()
        .map(|dir| dir.join(EXE))
        .find(|p| p.is_file())
}

/// Candidate Playnite install dirs, best first. LocalSystem invalidates the
/// obvious lookups:
///
/// - HKCU is SYSTEM's hive (`S-1-5-18`); read loaded `HKEY_USERS` instead
///   (logged-on streamers; same trade-off as [`crate::procscan::steam_running_hint`]).
/// - Match uninstall by `DisplayName`. Inno registers `<AppId>_is1`, not `Playnite`.
/// - `%LOCALAPPDATA%` is SYSTEM's profile; enumerate users-base profiles instead.
///
/// Portable installs leave only the `playnite://` handler
/// ([`uri_handler_dir`]). Registry `InstallLocation` before
/// conventional paths; each candidate is an `is_file` probe.
fn playnite_install_dirs() -> Vec<std::path::PathBuf> {
    use winreg::enums::{HKEY_LOCAL_MACHINE, HKEY_USERS, KEY_READ};
    use winreg::RegKey;

    // 64- and 32-bit views. HKCU/HKU `Software` is not redirected; WOW is HKLM-only.
    const UNINSTALL: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall";
    const UNINSTALL_WOW: &str = r"Software\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall";
    // `playnite://` command: bare inside a `…_Classes` hive, `Software\Classes` elsewhere.
    const URI_COMMAND: &str = r"playnite\shell\open\command";
    const CLASSES_URI_COMMAND: &str = r"Software\Classes\playnite\shell\open\command";

    let mut dirs: Vec<std::path::PathBuf> = Vec::new();

    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    playnite_dirs_from_uninstall(&hklm, UNINSTALL, &mut dirs);
    playnite_dirs_from_uninstall(&hklm, UNINSTALL_WOW, &mut dirs);
    uri_handler_dir(&hklm, CLASSES_URI_COMMAND, &mut dirs);

    let users = RegKey::predef(HKEY_USERS);
    for sid in users.enum_keys().flatten() {
        let Ok(hive) = users.open_subkey_with_flags(&sid, KEY_READ) else {
            continue;
        };
        // `_Classes` hives hold associations (`playnite://`), never uninstall keys.
        // Probe both spellings; a miss is one failed `open_subkey`.
        if sid.ends_with("_Classes") {
            uri_handler_dir(&hive, URI_COMMAND, &mut dirs);
            continue;
        }
        playnite_dirs_from_uninstall(&hive, UNINSTALL, &mut dirs);
        uri_handler_dir(&hive, CLASSES_URI_COMMAND, &mut dirs);
    }

    // Default per-user path, including profiles whose hive is not loaded.
    for profile in windows_user_profiles() {
        push_unique(&mut dirs, profile.join(r"AppData\Local\Playnite"));
    }
    dirs
}

/// Hydra's exe. Hydra registers `hydralauncher://` for its user on every start, wherever it is
/// installed; the per-user installer default covers a profile whose hive is not loaded.
fn hydra_exe() -> Option<std::path::PathBuf> {
    use winreg::enums::{HKEY_LOCAL_MACHINE, HKEY_USERS, KEY_READ};
    use winreg::RegKey;

    const URI_COMMAND: &str = r"hydralauncher\shell\open\command";
    const CLASSES_URI_COMMAND: &str = r"Software\Classes\hydralauncher\shell\open\command";

    let mut dirs = Vec::new();
    uri_handler_dir(
        &RegKey::predef(HKEY_LOCAL_MACHINE),
        CLASSES_URI_COMMAND,
        &mut dirs,
    );
    let users = RegKey::predef(HKEY_USERS);
    for sid in users.enum_keys().flatten() {
        if let Ok(hive) = users.open_subkey_with_flags(&sid, KEY_READ) {
            let path = if sid.ends_with("_Classes") {
                URI_COMMAND
            } else {
                CLASSES_URI_COMMAND
            };
            uri_handler_dir(&hive, path, &mut dirs);
        }
    }
    for profile in windows_user_profiles() {
        push_unique(
            &mut dirs,
            profile.join(r"AppData\Local\Programs\hydralauncher"),
        );
    }
    dirs.into_iter()
        .map(|dir| dir.join("Hydra.exe"))
        .find(|p| p.is_file())
}

/// `InstallLocation` from Playnite-looking uninstall entries under `root\path`.
/// Match `DisplayName` (`starts_with`) because the key is Inno's `<AppId>_is1`.
fn playnite_dirs_from_uninstall(
    root: &winreg::RegKey,
    path: &str,
    out: &mut Vec<std::path::PathBuf>,
) {
    use winreg::enums::KEY_READ;

    let Ok(uninstall) = root.open_subkey_with_flags(path, KEY_READ) else {
        return;
    };
    for name in uninstall.enum_keys().flatten() {
        let Ok(entry) = uninstall.open_subkey_with_flags(&name, KEY_READ) else {
            continue;
        };
        let display: String = entry.get_value("DisplayName").unwrap_or_default();
        if !display.starts_with("Playnite") {
            continue;
        }
        if let Ok(location) = entry.get_value::<String, _>("InstallLocation") {
            let location = location.trim();
            if !location.is_empty() {
                push_unique(out, std::path::PathBuf::from(location));
            }
        }
    }
}

/// Directory of the shell-open command at `root\path`. Finds a portable install (no
/// uninstall key, not under a profile). Same registration the launch path uses.
fn uri_handler_dir(root: &winreg::RegKey, path: &str, out: &mut Vec<std::path::PathBuf>) {
    use winreg::enums::KEY_READ;

    let Ok(command) = root
        .open_subkey_with_flags(path, KEY_READ)
        .and_then(|k| k.get_value::<String, _>(""))
    else {
        return;
    };
    if let Some(dir) = exe_from_shell_command(&command)
        .map(std::path::Path::new)
        .and_then(std::path::Path::parent)
        .filter(|d| !d.as_os_str().is_empty())
    {
        push_unique(out, dir.to_path_buf());
    }
}

/// Exe from a shell-open command. Quoted form first (what a registrar writes).
/// Unquoted fallback cuts at `.exe` — the path may contain spaces.
fn exe_from_shell_command(command: &str) -> Option<&str> {
    let command = command.trim();
    if let Some(rest) = command.strip_prefix('"') {
        return rest.split('"').next().filter(|p| !p.is_empty());
    }
    let end = command.to_ascii_lowercase().find(".exe")? + ".exe".len();
    Some(&command[..end])
}

/// Playnite install dirs as art roots. Portable keeps covers beside the exe
/// (`<PlayniteDir>\library\files\…`); the users base cannot see that tree.
/// Same shape as [`super::art::steam_art_roots`]. Candidates come from host
/// registry/fs probes, not the plugin lane that supplies the art path.
pub(crate) fn playnite_art_roots() -> Vec<std::path::PathBuf> {
    playnite_install_dirs()
        .into_iter()
        .filter(|d| d.is_dir())
        .collect()
}

/// User profiles (`C:\Users\*`), minus `Public`. Users base is `%PUBLIC%`'s
/// parent ([`super::art::art_roots`]); `%SystemDrive%\Users` if the var is missing.
fn windows_user_profiles() -> Vec<std::path::PathBuf> {
    let base = std::env::var_os("PUBLIC")
        .map(std::path::PathBuf::from)
        .and_then(|p| p.parent().map(std::path::Path::to_path_buf))
        .or_else(|| {
            std::env::var_os("SystemDrive").map(|d| std::path::PathBuf::from(d).join("Users"))
        });
    let Some(base) = base else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&base) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir() && !p.ends_with("Public"))
        .collect()
}

/// Dedup. Candidate lists are tiny; a linear scan beats a set.
fn push_unique(out: &mut Vec<std::path::PathBuf>, path: std::path::PathBuf) {
    if !out.contains(&path) {
        out.push(path);
    }
}

/// Epic URI from a `<namespace>:<catalogItemId>:<appName>` triple or a bare
/// `appName`. Charset-checked so the URI stays one argv token.
pub(crate) fn epic_launch_uri(value: &str) -> Option<String> {
    let ok = |s: &str| {
        !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    };
    let inner = match value.split(':').collect::<Vec<_>>().as_slice() {
        [ns, cat, app] if ok(ns) && ok(cat) && ok(app) => format!("{ns}%3A{cat}%3A{app}"),
        [app] if ok(app) => (*app).to_string(),
        _ => return None,
    };
    Some(format!(
        "com.epicgames.launcher://apps/{inner}?action=launch&silent=true"
    ))
}

/// GOG `exe \t args \t workdir` triple → `(cmdline, workdir)`. Direct spawn
/// (no Galaxy). Re-confine exe and workdir to [`gog_install_dirs`]: the plugin
/// already confined at parse, but the triple arrives over the provider API.
/// `None` if no GOG install owns the exe.
pub(crate) fn gog_spawn(value: &str) -> Option<(String, Option<PathBuf>)> {
    gog_spawn_in(value, &gog_install_dirs())
}

/// Pure [`gog_spawn`]: `installs` is the set the exe and workdir must sit inside.
fn gog_spawn_in(value: &str, installs: &[String]) -> Option<(String, Option<PathBuf>)> {
    let under = |p: &str| installs.iter().any(|dir| path_under(dir, p));
    let mut parts = value.split('\t');
    let exe = parts.next().filter(|s| !s.is_empty())?;
    if !under(exe) {
        tracing::warn!(
            exe,
            "gog launch: the exe is in no GOG install — refusing it"
        );
        return None;
    }
    let args = parts.next().unwrap_or("");
    // Out-of-bounds workdir is dropped, not refused: it only sets cwd of a confined exe.
    let workdir = parts.next().filter(|s| under(s)).map(PathBuf::from);
    let cmdline = if args.trim().is_empty() {
        format!("\"{exe}\"")
    } else {
        format!("\"{exe}\" {args}")
    };
    Some((cmdline, workdir))
}

/// GOG install roots from `HKLM\SOFTWARE\WOW6432Node\GOG.com\Games\<id>\PATH`.
/// GOG is 32-bit, so it writes the WOW view. Empty ⇒ every `gog` launch refuses.
fn gog_install_dirs() -> Vec<String> {
    use winreg::enums::HKEY_LOCAL_MACHINE;
    use winreg::RegKey;

    let Ok(games) =
        RegKey::predef(HKEY_LOCAL_MACHINE).open_subkey(r"SOFTWARE\WOW6432Node\GOG.com\Games")
    else {
        return Vec::new();
    };
    games
        .enum_keys()
        .flatten()
        .filter_map(|sub| {
            let path: String = games.open_subkey(&sub).ok()?.get_value("PATH").ok()?;
            let path = path.trim().to_string();
            (!path.is_empty()).then_some(path)
        })
        .collect()
}

/// A `gamebar` exe, run from its own folder, when `listed` names that exact path. The list
/// decides what may run; the plugin only picks from it.
fn gamebar_spawn_in(exe: &str, listed: &[String]) -> Option<WinRecipe> {
    if !valid_gamebar_exe(exe) {
        return None;
    }
    if !listed.iter().any(|l| l.eq_ignore_ascii_case(exe)) {
        tracing::warn!(
            exe,
            "gamebar launch: no signed-in user's Game Bar list names the exe — refusing it"
        );
        return None;
    }
    let workdir = Path::new(exe).parent().map(Path::to_path_buf);
    Some(WinRecipe::game(format!("\"{exe}\""), workdir))
}

/// A signed-in person's hive. Service hives are writable by their service, and the plugin
/// runner is LocalService (`S-1-5-19`).
fn user_sid(name: &str) -> bool {
    name.strip_prefix("S-1-5-21-").is_some_and(|rest| {
        !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit() || b == b'-')
    })
}

/// Every `MatchedExeFullPath` under each loaded user hive's `System\GameConfigStore\Children`:
/// the exes Game Bar recorded as games. A signed-out user's hive is not loaded.
fn gamebar_exes() -> Vec<String> {
    use winreg::enums::{HKEY_USERS, KEY_READ};
    use winreg::RegKey;

    let users = RegKey::predef(HKEY_USERS);
    users
        .enum_keys()
        .flatten()
        .filter(|sid| user_sid(sid))
        .filter_map(|sid| {
            users
                .open_subkey_with_flags(format!(r"{sid}\System\GameConfigStore\Children"), KEY_READ)
                .ok()
        })
        .flat_map(|children| {
            children
                .enum_keys()
                .flatten()
                .filter_map(|n| {
                    children
                        .open_subkey_with_flags(&n, KEY_READ)
                        .ok()?
                        .get_value::<String, _>("MatchedExeFullPath")
                        .ok()
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

/// Windows path containment: case-insensitive, either separator, `..` refused
/// (it climbs out of the prefix the test just accepted). String compare, not
/// [`Path::starts_with`], which is case-sensitive — plugin spelling may differ.
fn path_under(dir: &str, path: &str) -> bool {
    let norm = |s: &str| s.replace('/', "\\").trim_end_matches('\\').to_lowercase();
    let (dir, path) = (norm(dir), norm(path));
    if dir.is_empty() || path.split('\\').any(|c| c == "..") {
        return false;
    }
    path == dir
        || path
            .strip_prefix(&dir)
            .is_some_and(|rest| rest.starts_with('\\'))
}

/// Launches an operator `apps.json` command as the host's WTS session user.
///
/// Capture is already live and the host remains SYSTEM. Linux instead uses
/// compositor-aware [`launch_session_command`].
pub fn launch_gamestream_command(cmd: &str) -> Result<WindowsLaunch> {
    let cmd = cmd.trim();
    anyhow::ensure!(!cmd.is_empty(), "empty command");
    let pid = crate::interactive::spawn_as_current_session_user(&format!("cmd.exe /c {cmd}"), None)
        .context("spawn gamestream command as the current WTS session user")?;
    tracing::info!(command = %cmd, pid, "gamestream: launched app as the current WTS session user");
    // `cmd.exe /c` waits, so its PID tracks the command; forwarders remain the lease shim's concern.
    Ok(WindowsLaunch {
        pid,
        owns_game: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launcher_ui_knows_the_windows_launchers_and_probes_each() {
        // Vocabulary vs installed: separate so a missing launcher cannot 400 the library.
        for v in ["playnite", "epic", "gog", "xbox", "hydra-big-picture"] {
            assert!(known_launcher_ui(v), "{v}");
        }
        assert_eq!(
            resolvable_launcher_ui("playnite"),
            playnite_fullscreen_exe().is_some()
        );
        assert_eq!(resolvable_launcher_ui("gog"), galaxy_exe().is_some());
        assert_eq!(
            resolvable_launcher_ui("hydra-big-picture"),
            hydra_exe().is_some()
        );
        assert!(!known_launcher_ui("heroic"));
        assert!(!known_launcher_ui("lutris"));
    }

    /// Store clients forward to a running instance, so none owns the session.
    #[test]
    fn store_launchers_are_handoffs() {
        let ui = |v: &str| {
            windows_launch_for(&LaunchSpec {
                kind: "launcher_ui".into(),
                value: v.into(),
                ..Default::default()
            })
        };
        if let Some(r) = ui("xbox") {
            assert!(
                r.cmdline.ends_with("!Microsoft.Xbox.App\""),
                "{}",
                r.cmdline
            );
            assert!(!r.owns_game);
        }
        if let Some(r) = ui("gog") {
            assert!(r.cmdline.ends_with("GalaxyClient.exe\""), "{}", r.cmdline);
            assert!(!r.owns_game);
        }
        if let Some(r) = ui("hydra-big-picture") {
            assert!(
                r.cmdline.ends_with("Hydra.exe\" --big-picture"),
                "{}",
                r.cmdline
            );
            assert!(!r.owns_game);
        }
    }

    /// Parse `playnite://` command lines (portable probe, works off-Windows).
    /// A miss is a portable install the host cannot find — no tile, no covers.
    #[test]
    fn exe_is_read_out_of_a_registered_shell_command() {
        assert_eq!(
            exe_from_shell_command(r#""D:\Apps\Playnite\Playnite.DesktopApp.exe" --uridata "%1""#),
            Some(r"D:\Apps\Playnite\Playnite.DesktopApp.exe")
        );
        // Unquoted path with spaces: cannot split on whitespace.
        assert_eq!(
            exe_from_shell_command(r"C:\Program Files\Playnite\Playnite.DesktopApp.exe %1"),
            Some(r"C:\Program Files\Playnite\Playnite.DesktopApp.exe")
        );
        assert_eq!(
            exe_from_shell_command(r"D:\Apps\Playnite\PLAYNITE.DESKTOPAPP.EXE"),
            Some(r"D:\Apps\Playnite\PLAYNITE.DESKTOPAPP.EXE")
        );
        // No exe shape / empty quotes: a bogus candidate would become an art root.
        assert_eq!(
            exe_from_shell_command("rundll32 shell32.dll,Control_RunDLL"),
            None
        );
        assert_eq!(exe_from_shell_command(r#""" %1"#), None);
        assert_eq!(exe_from_shell_command(""), None);
    }

    /// Launcher tile is Fullscreen. `playnite://` is registered to the desktop app.
    #[test]
    fn playnite_launcher_opens_the_fullscreen_app() {
        let ui = |v: &str| {
            windows_launch_for(&LaunchSpec {
                kind: "launcher_ui".into(),
                value: v.into(),
                ..Default::default()
            })
        };
        assert!(ui("lutris").is_none());
        assert!(ui("heroic").is_none());
        assert!(ui("").is_none());

        let Some(exe) = playnite_fullscreen_exe() else {
            return;
        };
        let r = ui("playnite").expect("resolvable when the exe was found");
        let cmd = &r.cmdline;
        assert!(cmd.contains("Playnite.FullscreenApp.exe"), "{cmd}");
        assert!(!cmd.contains("DesktopApp"), "{cmd}");
        assert!(!cmd.contains("playnite://"), "{cmd}");
        assert_eq!(r.workdir.as_deref(), exe.parent());
        assert!(r.owns_game, "the exe is spawned directly, not forwarded");
    }
    #[test]
    fn steam_ui_resolves_to_the_client_ui_on_windows() {
        let ui = |v: &str| {
            windows_launch_for(&LaunchSpec {
                kind: "steam_ui".into(),
                value: v.into(),
                ..Default::default()
            })
        };
        let bp = ui("bigpicture").expect("bigpicture recipe");
        assert!(
            bp.cmdline.contains("steam://open/bigpicture"),
            "line was {:?}",
            bp.cmdline
        );
        assert!(bp.workdir.is_none());
        assert!(!bp.owns_game, "a steam:// URI is forwarded to the client");
        let desk = ui("desktop").expect("desktop recipe");
        assert!(
            desk.cmdline.contains("steam://open/main"),
            "line was {:?}",
            desk.cmdline
        );
        assert!(ui("nonsense").is_none());
        assert!(ui("").is_none());
    }
    #[test]
    fn epic_launch_uri_triple_bare_and_guard() {
        assert_eq!(
            epic_launch_uri("fn:abc:Fortnite").as_deref(),
            Some("com.epicgames.launcher://apps/fn%3Aabc%3AFortnite?action=launch&silent=true")
        );
        assert_eq!(
            epic_launch_uri("Fortnite").as_deref(),
            Some("com.epicgames.launcher://apps/Fortnite?action=launch&silent=true")
        );
        assert!(epic_launch_uri("bad part:x:y").is_none());
        assert!(epic_launch_uri("").is_none());
    }
    #[test]
    fn gog_spawn_parses_and_guards() {
        let installs = ["C:\\Games\\W3".to_string(), "C:\\".to_string()];
        let spawn = |v: &str| gog_spawn_in(v, &installs);
        let (cmd, wd) = spawn("C:\\Games\\W3\\witcher3.exe\t--skip\tC:\\Games\\W3").unwrap();
        assert_eq!(cmd, "\"C:\\Games\\W3\\witcher3.exe\" --skip");
        assert_eq!(wd, Some(std::path::PathBuf::from("C:\\Games\\W3")));
        let (cmd2, wd2) = spawn("C:\\g.exe").unwrap();
        assert_eq!(cmd2, "\"C:\\g.exe\"");
        assert!(wd2.is_none());
        assert!(spawn("").is_none());
    }
    /// The exe arrives over the provider API: Game Bar's list, read by the host, decides.
    #[test]
    fn gamebar_spawn_runs_only_a_listed_exe_from_its_folder() {
        let listed = [r"D:\Games\Hollow Knight\hollow_knight.exe".to_string()];
        let r = gamebar_spawn_in(r"d:\games\hollow knight\HOLLOW_KNIGHT.exe", &listed).unwrap();
        assert_eq!(r.cmdline, r#""d:\games\hollow knight\HOLLOW_KNIGHT.exe""#);
        assert_eq!(r.workdir, Some(PathBuf::from(r"d:\games\hollow knight")));
        assert!(r.owns_game);
        // Not on the list, or on it but no longer one quoted argv element: nothing runs.
        assert!(gamebar_spawn_in(r"C:\Windows\System32\cmd.exe", &listed).is_none());
        assert!(gamebar_spawn_in(r"D:\Games\Hollow Knight\hollow_knight.exe", &[]).is_none());
        let quoted = [r#"C:\x.exe" /c calc"#.to_string()];
        assert!(gamebar_spawn_in(&quoted[0], &quoted).is_none());
    }

    #[test]
    fn gamebar_reads_only_the_hives_of_people() {
        assert!(user_sid("S-1-5-21-2040749213-3813789928-2098397110-1001"));
        for sid in [
            "S-1-5-19",
            "S-1-5-18",
            ".DEFAULT",
            "S-1-5-21-",
            "S-1-5-21-2040749213-3813789928-2098397110-1001_Classes",
        ] {
            assert!(!user_sid(sid), "{sid}");
        }
    }

    /// Triple arrives over the provider API: exe must sit in a host-found GOG install.
    #[test]
    fn gog_spawn_refuses_an_exe_outside_every_gog_install() {
        let installs = ["C:\\Games\\W3".to_string()];
        let spawn = |v: &str| gog_spawn_in(v, &installs);
        assert!(spawn("C:\\Windows\\System32\\cmd.exe\t/c calc\tC:\\Games\\W3").is_none());
        // Prefix match is not containment: a sibling path must not pass.
        assert!(spawn("C:\\Games\\W3x\\evil.exe").is_none());
        assert!(spawn("C:\\Games\\W3\\..\\..\\Windows\\System32\\cmd.exe").is_none());
        // Empty install set ⇒ nothing is launchable, rather than everything.
        assert!(gog_spawn_in("C:\\Games\\W3\\witcher3.exe", &[]).is_none());
        // Case and separator: plugin spelling may differ from the registry.
        assert!(spawn("c:/games/w3/bin/game.exe").is_some());
        // Out-of-bounds workdir costs the workdir, not the launch.
        let (_, wd) = spawn("C:\\Games\\W3\\witcher3.exe\t\tC:\\Windows\\System32").unwrap();
        assert!(wd.is_none());
    }
    /// Family name from PackageFullName: the `xbox` kind's Identity completion.
    #[test]
    fn pfn_reduces_a_package_full_name_to_its_family() {
        assert_eq!(
            pfn_from_full(
                "Microsoft.624F8B84B80_1.0.0.0_x64__8wekyb3d8bbwe",
                "Microsoft.624F8B84B80"
            )
            .as_deref(),
            Some("Microsoft.624F8B84B80_8wekyb3d8bbwe")
        );
        // No `_` → nothing to reduce; must not invent a hash.
        assert!(pfn_from_full("NoUnderscore", "NoUnderscore").is_none());
    }

    #[test]
    fn a_seat_starts_its_own_steam_under_its_own_name() {
        let line = seat_steam_cmdline(
            std::path::Path::new(r"C:\Users\pf_seat1\Steam\steam.exe"),
            "0123456789abcdef0123456789abcdef",
            "steam://rungameid/570",
        );
        assert_eq!(
            line,
            r#""C:\Users\pf_seat1\Steam\steam.exe" -master_ipc_name_override pfseat0123456789abcdef0123456789abcdef "steam://rungameid/570""#
        );
    }

    #[test]
    fn windows_launch_for_maps_and_guards() {
        let steam = LaunchSpec {
            kind: "steam_appid".into(),
            value: "570".into(),
            ..Default::default()
        };
        let steam_r = windows_launch_for(&steam).expect("steam recipe");
        let line = &steam_r.cmdline;
        assert!(line.contains("steam://rungameid/570"), "line was {line:?}");
        assert!(steam_r.workdir.is_none());
        let evil = LaunchSpec {
            kind: "steam_appid".into(),
            value: "570\" & calc".into(),
            ..Default::default()
        };
        assert!(windows_launch_for(&evil).is_none());
        let cmd = LaunchSpec {
            kind: "command".into(),
            value: "notepad.exe".into(),
            ..Default::default()
        };
        let cmd_r = windows_launch_for(&cmd).unwrap();
        assert_eq!(cmd_r.cmdline, "cmd.exe /c notepad.exe");
        assert!(
            cmd_r.owns_game,
            "`cmd /c` blocks on the operator's command, so its pid is that command's"
        );
        let aumid = LaunchSpec {
            kind: "aumid".into(),
            value: "Microsoft.X_8wekyb3d8bbwe!Game".into(),
            ..Default::default()
        };
        assert_eq!(
            windows_launch_for(&aumid).unwrap().cmdline,
            "explorer.exe \"shell:AppsFolder\\Microsoft.X_8wekyb3d8bbwe!Game\""
        );
        assert!(windows_launch_for(&LaunchSpec {
            kind: "aumid".into(),
            value: "no-bang".into(),
            args: None,
        })
        .is_none());
        assert!(windows_launch_for(&LaunchSpec {
            kind: "command".into(),
            value: "  ".into(),
            args: None,
        })
        .is_none());
        assert!(windows_launch_for(&LaunchSpec {
            kind: "wat".into(),
            value: "x".into(),
            args: None,
        })
        .is_none());
        let uplay = windows_launch_for(&LaunchSpec {
            kind: "uplay".into(),
            value: "5595".into(),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(uplay.cmdline, "explorer.exe \"uplay://launch/5595/0\"");
        assert!(!uplay.owns_game, "a protocol hand-off is not the game");
        let amazon = windows_launch_for(&LaunchSpec {
            kind: "amazon".into(),
            value: "amzn1.adg.product.abc-123".into(),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(
            amazon.cmdline,
            "explorer.exe \"amazon-games://play/amzn1.adg.product.abc-123\""
        );
        assert!(windows_launch_for(&LaunchSpec {
            kind: "uplay".into(),
            value: "5595\" & calc".into(),
            args: None,
        })
        .is_none());
        assert!(windows_launch_for(&LaunchSpec {
            kind: "battlenet".into(),
            value: "WTCG\" & calc".into(),
            args: None,
        })
        .is_none());
        let ea = windows_launch_for(&LaunchSpec {
            kind: "ea".into(),
            value: "Origin.SFT.50.0000123".into(),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(
            ea.cmdline,
            "explorer.exe \"origin2://game/launch/?offerIds=Origin.SFT.50.0000123\""
        );
        assert!(!ea.owns_game);
        assert!(windows_launch_for(&LaunchSpec {
            kind: "rockstar".into(),
            value: "gta5\" & calc".into(),
            args: None,
        })
        .is_none());
    }
}
