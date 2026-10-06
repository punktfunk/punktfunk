//! Profiles: the people on this box, in `profiles.json` beside the pairing store.
//!
//! The schema is `multi-user-profiles.md` §4 with one more account kind, [`OsAccount::Seat`].
//! The owner profile is the box's own session; [`Profiles::ensure_owner`] creates it at start.
//! A profile is not a trust boundary: the device stays the principal, and any paired device may
//! name any profile.
//!
//! Two file rules hold. A file that does not parse keeps its bytes and refuses every mutation, so
//! a save never overwrites what could not be read. A save that fails is a mutation that failed:
//! memory keeps the old file.

use crate::library::CustomEntry;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};
use utoipa::ToSchema;

/// Bumped on a breaking change to a field's meaning; `serde(default)` covers additions.
pub const PROFILES_SCHEMA_VERSION: u32 = 1;
/// The longest profile id any wire or ABI carries.
pub const PROFILE_ID_MAX: usize = 64;
/// Profiles on one box, the owner included.
pub const PROFILES_MAX: usize = 8;
/// Id of the owner when no stored owner exists, as when `profiles.json` did not parse.
pub const OPERATOR_PROFILE_ID: &str = "operator";
/// A display name's length after trimming.
pub const DISPLAY_NAME_MAX: usize = 32;
/// The largest avatar the host stores.
pub const AVATAR_MAX_BYTES: usize = 1024 * 1024;
const OWNER_ACCENT: &str = "#3b82f6";

/// Host-minted, 12 lowercase hex.
pub type ProfileId = String;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ProfilesFile {
    /// `0` or absent reads as 1.
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub profiles: Vec<Profile>,
    /// Where a device that names no profile lands. `None` or dangling: the owner.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_profile_id: Option<ProfileId>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Profile {
    pub id: ProfileId,
    pub display_name: String,
    /// `#RRGGBB` behind the initials when there is no picture.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accent: Option<String>,
    /// Host-relative URL of the stored picture.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar: Option<String>,
    pub os_account: OsAccount,
    /// What a bare connect opens.
    #[serde(default)]
    pub home: Home,
    /// The device fingerprint whose seat home became this profile. Lowercase hex.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub legacy_device: Option<String>,
    #[serde(default)]
    pub assigned_fingerprints: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passcode: Option<PasscodeHash>,
    #[serde(default)]
    pub require_passcode_even_when_assigned: bool,
    #[serde(default)]
    pub allow_shared_view: bool,
    #[serde(default)]
    pub tvos_user_ids: Vec<String>,
    #[serde(default)]
    pub library_scope: LibraryScope,
    #[serde(default)]
    pub custom_entries: Vec<CustomEntry>,
    #[serde(default)]
    pub session_defaults: SessionDefaults,
    #[serde(default)]
    pub created_unix: u64,
    #[serde(default)]
    pub updated_unix: u64,
    /// When a session last resolved to this profile. `0`: never.
    #[serde(default)]
    pub last_used_unix: u64,
}

/// Where a profile's session runs.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OsAccount {
    /// The box's own session.
    Operator,
    /// A seat of the box. On Linux `seat` is absent and the home is `seats/<id>`; on Windows it
    /// is the seats ledger's id.
    Seat {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        seat: Option<String>,
        #[serde(default)]
        tier: SeatTier,
    },
    /// Reserved for the OS-user increment: resolves to [`ProfileError::SessionUnavailable`].
    Linux {
        username: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        uid: Option<u32>,
    },
    /// Reserved for the OS-user increment: resolves to [`ProfileError::SessionUnavailable`].
    Windows {
        account_name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sid: Option<String>,
        credential: CredentialRef,
    },
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SeatTier {
    /// A Steam home in a gamescope of its own, under the box owner's uid.
    #[default]
    Light,
    /// A user of its own with its own session and host.
    Full,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Home {
    /// The session as it is.
    #[default]
    Desktop,
    /// Steam Big Picture in the profile's seat.
    Bigpicture,
}

/// Names a secret in an OS vault; never the secret.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum CredentialRef {
    None,
    CredentialManager { target: String },
    LsaSecret { key: String },
    DpapiBlob { path: String },
}

/// Argon2id PHC string. Unused in this increment.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PasscodeHash {
    pub phc: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum LibraryScope {
    #[default]
    All,
    Allow {
        ids: Vec<String>,
    },
    Deny {
        ids: Vec<String>,
    },
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct SessionDefaults {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_height: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_refresh_hz: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bitrate_kbps: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compositor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gamepad: Option<String>,
}

/// The profile a connect lands in.
#[derive(Clone, Debug, PartialEq)]
pub struct Resolved {
    pub id: ProfileId,
    pub display_name: String,
    pub os_account: OsAccount,
    pub home: Home,
    pub via: ResolveVia,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResolveVia {
    /// The device asked for it.
    Asked,
    /// The device's old seat home became this profile.
    LegacyDevice,
    /// `default_profile_id`, or the owner.
    Default,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProfileError {
    /// No profile has that id.
    Unknown,
    /// A seat profile asked of another seat's host.
    NotThisSeat,
    /// The profile's account needs a session broker this host does not have.
    SessionUnavailable,
}

/// Why an edit was refused, each one HTTP status.
#[derive(Debug)]
pub enum EditError {
    /// No profile with that id.
    NotFound,
    /// The body breaks a rule; the sentence says which.
    Invalid(String),
    /// Another profile has that name.
    NameTaken,
    /// The box already has [`PROFILES_MAX`].
    Full,
    /// The owner profile is the box's own session; it can't be removed.
    Owner,
    /// The file did not parse, or the save failed.
    Store(anyhow::Error),
}

impl std::fmt::Display for EditError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EditError::NotFound => f.write_str("no profile with that id"),
            EditError::Invalid(why) => f.write_str(why),
            EditError::NameTaken => f.write_str("another profile has that name"),
            EditError::Full => write!(f, "a host holds at most {PROFILES_MAX} profiles"),
            EditError::Owner => f.write_str("the owner profile can't be removed"),
            EditError::Store(e) => write!(f, "{e:#}"),
        }
    }
}

/// What a new profile starts as.
#[derive(Clone, Debug, Deserialize, ToSchema)]
pub struct ProfileCreate {
    pub display_name: String,
    /// `#RRGGBB`.
    #[serde(default)]
    pub accent: Option<String>,
    /// Defaults to `bigpicture` for a seat profile, `desktop` otherwise.
    #[serde(default)]
    pub home: Option<Home>,
    /// A seat of its own (the default), or the box's own session under another name.
    #[serde(default = "yes")]
    pub seat: bool,
}

fn yes() -> bool {
    true
}

/// A change to a profile; absent fields stay.
#[derive(Clone, Debug, Default, Deserialize, ToSchema)]
pub struct ProfileUpdate {
    #[serde(default)]
    pub display_name: Option<String>,
    /// `#RRGGBB`.
    #[serde(default)]
    pub accent: Option<String>,
    #[serde(default)]
    pub home: Option<Home>,
}

/// Ids of removed profiles, for the sessions playing as them.
static REMOVED: std::sync::OnceLock<tokio::sync::broadcast::Sender<ProfileId>> =
    std::sync::OnceLock::new();

/// Hears every profile removed from now on.
pub fn removed() -> tokio::sync::broadcast::Receiver<ProfileId> {
    REMOVED
        .get_or_init(|| tokio::sync::broadcast::channel(16).0)
        .subscribe()
}

/// A stored picture.
#[derive(Clone, Debug, PartialEq)]
pub struct Avatar {
    pub bytes: Vec<u8>,
    pub mime: &'static str,
    /// Hex SHA-256 prefix of `bytes`.
    pub etag: String,
}

struct State {
    path: PathBuf,
    file: ProfilesFile,
    /// The bytes of a file that did not parse. While set, every mutation is refused.
    unparsed: Option<Vec<u8>>,
    /// Lowercase `legacy_device` fingerprint → profile id.
    legacy: HashMap<String, ProfileId>,
    /// The box's file on a seat host: never written, re-read when its mtime moves.
    read_only: bool,
    stamp: Option<std::time::SystemTime>,
}

/// The profile store. One per host process, shared like the pairing store.
pub struct Profiles {
    state: Mutex<State>,
    /// This host's seat id when it is a seat host; `None` on the box's own host.
    seat: Option<String>,
}

impl Profiles {
    /// `path` `None` is `profiles.json` in the config dir.
    pub fn load_with(path: Option<PathBuf>, seat: Option<String>) -> Profiles {
        let path = path.unwrap_or_else(|| pf_paths::config_dir().join("profiles.json"));
        let (file, unparsed) = read(&path);
        let legacy = legacy_index(&file);
        Profiles {
            state: Mutex::new(State {
                path,
                file,
                unparsed,
                legacy,
                read_only: false,
                stamp: None,
            }),
            seat,
        }
    }

    /// A seat host's view of the box's `profiles.json` in its trust dir: read only, followed
    /// as the box console edits it. `seat` is this host's seat id.
    pub fn load_box(path: PathBuf, seat: Option<String>) -> Profiles {
        let (file, unparsed) = parse(&path, std::fs::read(&path).ok());
        Profiles {
            state: Mutex::new(State {
                stamp: mtime(&path),
                legacy: legacy_index(&file),
                path,
                file,
                unparsed,
                read_only: true,
            }),
            seat,
        }
    }

    pub fn list(&self) -> Vec<Profile> {
        self.lock().file.profiles.clone()
    }

    /// The owner's id: the box's own session.
    pub fn owner_id(&self) -> Option<ProfileId> {
        let state = self.lock();
        state
            .file
            .profiles
            .iter()
            .find(|p| is_owner(p))
            .map(|p| p.id.clone())
    }

    pub fn default_profile_id(&self) -> Option<ProfileId> {
        default_profile(&self.lock().file).map(|p| p.id.clone())
    }

    /// `seat` is the ledger row a Windows seat profile plays on; a Linux seat needs none.
    pub fn create(&self, input: ProfileCreate, seat: Option<String>) -> Result<Profile, EditError> {
        let name = clean_name(&input.display_name)
            .ok_or_else(|| EditError::Invalid("a profile needs a name".into()))?;
        let accent = input.accent.map(valid_accent).transpose()?;
        let mut made = None;
        self.edit(|file| {
            if file.profiles.len() >= PROFILES_MAX {
                return Err(EditError::Full);
            }
            if name_taken(&file.profiles, &name, None) {
                return Err(EditError::NameTaken);
            }
            let now = now_unix();
            let (os_account, home) = if input.seat {
                // A Windows seat is a desktop of its own; a Linux one is a Steam home.
                let (tier, home) = match &seat {
                    Some(_) => (SeatTier::Full, Home::Desktop),
                    None => (SeatTier::Light, Home::Bigpicture),
                };
                let account = OsAccount::Seat {
                    seat: seat.clone(),
                    tier,
                };
                (account, input.home.unwrap_or(home))
            } else {
                (OsAccount::Operator, input.home.unwrap_or(Home::Desktop))
            };
            let profile = Profile {
                id: new_id(&name),
                display_name: name.clone(),
                accent: accent.clone(),
                avatar: None,
                os_account,
                home,
                legacy_device: None,
                assigned_fingerprints: Vec::new(),
                passcode: None,
                require_passcode_even_when_assigned: false,
                allow_shared_view: false,
                tvos_user_ids: Vec::new(),
                library_scope: LibraryScope::All,
                custom_entries: Vec::new(),
                session_defaults: SessionDefaults::default(),
                created_unix: now,
                updated_unix: now,
                last_used_unix: 0,
            };
            file.profiles.push(profile.clone());
            made = Some(profile);
            Ok(())
        })?;
        made.ok_or(EditError::NotFound)
    }

    /// Rename, recolour, rehome. A rename drops `legacy_device`: the name was the device's.
    pub fn update(&self, id: &str, input: ProfileUpdate) -> Result<Profile, EditError> {
        let name = input
            .display_name
            .as_deref()
            .map(|n| {
                clean_name(n).ok_or_else(|| EditError::Invalid("a profile needs a name".into()))
            })
            .transpose()?;
        let accent = input.accent.map(valid_accent).transpose()?;
        let mut out = None;
        self.edit(|file| {
            if let Some(name) = &name {
                if name_taken(&file.profiles, name, Some(id)) {
                    return Err(EditError::NameTaken);
                }
            }
            let p = file
                .profiles
                .iter_mut()
                .find(|p| p.id == id)
                .ok_or(EditError::NotFound)?;
            if let Some(name) = name {
                if name != p.display_name {
                    p.legacy_device = None;
                }
                p.display_name = name;
            }
            if let Some(accent) = accent {
                p.accent = Some(accent);
            }
            if let Some(home) = input.home {
                p.home = home;
            }
            p.updated_unix = now_unix();
            out = Some(p.clone());
            Ok(())
        })?;
        out.ok_or(EditError::NotFound)
    }

    /// Removes `id`. `erase` also deletes its Steam home and picture; without it the home stays
    /// for the console to list. Sessions playing as it hear [`removed`].
    pub fn delete(&self, id: &str, erase: bool) -> Result<Profile, EditError> {
        let mut gone = None;
        self.edit(|file| {
            let at = file
                .profiles
                .iter()
                .position(|p| p.id == id)
                .ok_or(EditError::NotFound)?;
            if file
                .profiles
                .iter()
                .find(|p| is_owner(p))
                .map(|p| p.id.as_str())
                == Some(id)
            {
                return Err(EditError::Owner);
            }
            if file.default_profile_id.as_deref() == Some(id) {
                file.default_profile_id = None;
            }
            gone = Some(file.profiles.remove(at));
            Ok(())
        })?;
        let gone = gone.ok_or(EditError::NotFound)?;
        if erase && safe_id(id) {
            let state = self.lock();
            remove_avatar_files(&state.path.with_file_name("profiles"), id, None);
            drop(state);
            #[cfg(target_os = "linux")]
            {
                let _ = std::fs::remove_dir_all(pf_paths::seat_home(id));
                let _ = std::fs::remove_file(pf_paths::seat_record(id));
            }
        }
        if let Some(tx) = REMOVED.get() {
            let _ = tx.send(id.to_string());
        }
        Ok(gone)
    }

    /// Where a device that names no profile lands; `None` is the owner.
    pub fn set_default(&self, id: Option<&str>) -> Result<(), EditError> {
        self.edit(|file| {
            if let Some(id) = id {
                if !file.profiles.iter().any(|p| p.id == id) {
                    return Err(EditError::NotFound);
                }
            }
            file.default_profile_id = id.map(str::to_string);
            Ok(())
        })
    }

    /// Stamps `last_used_unix`. Best-effort: a connect never fails on it.
    pub fn touch(&self, id: &str) {
        let known = self.lock().file.profiles.iter().any(|p| p.id == id);
        if known {
            let _ = self.mutate(|file| {
                if let Some(p) = file.profiles.iter_mut().find(|p| p.id == id) {
                    p.last_used_unix = now_unix();
                }
                Ok(())
            });
        }
    }

    /// [`Self::mutate`] with the edit's own error kinds.
    fn edit(
        &self,
        change: impl FnOnce(&mut ProfilesFile) -> Result<(), EditError>,
    ) -> Result<(), EditError> {
        let mut refused = None;
        let saved = self.mutate(|file| {
            change(file).map_err(|e| {
                let msg = e.to_string();
                refused = Some(e);
                anyhow::anyhow!(msg)
            })
        });
        match (refused, saved) {
            (Some(e), _) => Err(e),
            (None, Ok(())) => Ok(()),
            (None, Err(e)) => Err(EditError::Store(e)),
        }
    }

    pub fn get(&self, id: &str) -> Option<Profile> {
        self.lock()
            .file
            .profiles
            .iter()
            .find(|p| p.id == id)
            .cloned()
    }

    /// Creates the owner profile when none exists; a no-op after that. Named per D-M: the
    /// account's full name, then the console user on Windows, then `fallback`.
    pub fn ensure_owner(&self, fallback: &str) -> Result<()> {
        if self.lock().file.profiles.iter().any(is_owner) {
            return Ok(());
        }
        let name = os_display_name()
            .and_then(|n| clean_name(&n))
            .or_else(|| clean_name(fallback))
            .unwrap_or_else(|| "Owner".to_string());
        self.mutate(|file| {
            if file.profiles.iter().any(is_owner) {
                return Ok(());
            }
            let now = now_unix();
            let display_name = unique_name(&file.profiles, &name);
            file.profiles.insert(
                0,
                Profile {
                    id: new_id(&display_name),
                    display_name,
                    accent: Some(OWNER_ACCENT.to_string()),
                    avatar: None,
                    os_account: OsAccount::Operator,
                    home: Home::Desktop,
                    legacy_device: None,
                    assigned_fingerprints: Vec::new(),
                    passcode: None,
                    require_passcode_even_when_assigned: false,
                    allow_shared_view: false,
                    tvos_user_ids: Vec::new(),
                    library_scope: LibraryScope::All,
                    custom_entries: Vec::new(),
                    session_defaults: SessionDefaults::default(),
                    created_unix: now,
                    updated_unix: now,
                    last_used_unix: 0,
                },
            );
            Ok(())
        })
    }

    /// Turns each device seat under `seats` (`seats/<8 hex>` holding a Steam) into a seat
    /// profile: named after the paired device whose fingerprint starts with those digits, with
    /// that device as `legacy_device`, its home and record renamed to the profile id. A
    /// directory whose device is gone becomes `Seat <8 hex>`. `paired` is `(name, fingerprint)`.
    pub fn migrate_device_seats(&self, seats: &Path, paired: &[(String, String)]) {
        let Ok(dir) = std::fs::read_dir(seats) else {
            return;
        };
        let mut found: Vec<String> = dir
            .flatten()
            .filter_map(|e| e.file_name().to_str().map(str::to_string))
            .filter(|n| n.len() == 8 && n.bytes().all(|b| b.is_ascii_hexdigit()))
            .filter(|n| seats.join(n).join(".local/share/Steam/steam.sh").exists())
            .collect();
        found.sort();
        for hex in found {
            let device = paired.iter().find(|(_, fp)| {
                fp.to_ascii_lowercase()
                    .starts_with(&hex.to_ascii_lowercase())
            });
            let name = device
                .and_then(|(n, _)| clean_name(n))
                .unwrap_or_else(|| format!("Seat {hex}"));
            let legacy = device.map(|(_, fp)| fp.to_ascii_lowercase());
            match self.migrate_one(seats, &hex, &name, legacy) {
                Ok(id) => {
                    tracing::info!(seat = %hex, profile = %id, %name, "device seat moved to a profile")
                }
                Err(e) => {
                    tracing::warn!(seat = %hex, error = %format!("{e:#}"), "device seat not migrated")
                }
            }
        }
    }

    fn migrate_one(
        &self,
        seats: &Path,
        hex: &str,
        name: &str,
        legacy: Option<String>,
    ) -> Result<ProfileId> {
        let mut id = String::new();
        self.mutate(|file| {
            if file.profiles.len() >= PROFILES_MAX {
                bail!("the box already has {PROFILES_MAX} profiles");
            }
            let now = now_unix();
            let display_name = unique_name(&file.profiles, name);
            id = new_id(&display_name);
            file.profiles.push(Profile {
                id: id.clone(),
                display_name,
                accent: None,
                avatar: None,
                os_account: OsAccount::Seat {
                    seat: None,
                    tier: SeatTier::Light,
                },
                home: Home::Bigpicture,
                legacy_device: legacy,
                assigned_fingerprints: Vec::new(),
                passcode: None,
                require_passcode_even_when_assigned: false,
                allow_shared_view: false,
                tvos_user_ids: Vec::new(),
                library_scope: LibraryScope::All,
                custom_entries: Vec::new(),
                session_defaults: SessionDefaults::default(),
                created_unix: now,
                updated_unix: now,
                last_used_unix: 0,
            });
            Ok(())
        })?;
        let moved = std::fs::rename(seats.join(hex), seats.join(&id));
        if let Err(e) = moved {
            // A profile whose home stayed behind would find no Steam; take it back.
            let _ = self.mutate(|file| {
                file.profiles.retain(|p| p.id != id);
                Ok(())
            });
            return Err(e).context("rename the seat home");
        }
        let record = seats.join(format!("{hex}.json"));
        if record.exists() {
            std::fs::rename(&record, seats.join(format!("{id}.json")))
                .context("rename the seat record")?;
        }
        Ok(id)
    }

    /// The profile a connect from `fp` asking for `asked` lands in (`profiles-and-seats.md`
    /// §6.2). `asked` is read before trust: it is only ever compared against stored ids.
    pub fn resolve(&self, fp: Option<&str>, asked: Option<&str>) -> Result<Resolved, ProfileError> {
        let state = self.lock();
        let file = &state.file;
        let (profile, via) = match asked {
            Some(id) => {
                let found = (id.len() <= PROFILE_ID_MAX)
                    .then(|| file.profiles.iter().find(|p| p.id == id))
                    .flatten()
                    .ok_or(ProfileError::Unknown)?;
                if let (Some(mine), OsAccount::Seat { seat, .. }) = (&self.seat, &found.os_account)
                {
                    if seat.as_ref() != Some(mine) {
                        return Err(ProfileError::NotThisSeat);
                    }
                }
                (Some(found), ResolveVia::Asked)
            }
            // A seat host serves one profile; the box's default is someone else's.
            None if self.seat.is_some() => {
                let mine = file.profiles.iter().find(
                    |p| matches!(&p.os_account, OsAccount::Seat { seat, .. } if seat == &self.seat),
                );
                // No profile names this seat any more: it plays for nobody.
                (
                    Some(mine.ok_or(ProfileError::SessionUnavailable)?),
                    ResolveVia::Default,
                )
            }
            None => {
                let legacy = fp
                    .and_then(|fp| state.legacy.get(&fp.to_ascii_lowercase()))
                    .and_then(|id| file.profiles.iter().find(|p| &p.id == id));
                match legacy {
                    Some(p) => (Some(p), ResolveVia::LegacyDevice),
                    None => (default_profile(file), ResolveVia::Default),
                }
            }
        };
        let Some(profile) = profile else {
            return Ok(Resolved {
                id: OPERATOR_PROFILE_ID.to_string(),
                display_name: crate::host::machine_hostname(),
                os_account: OsAccount::Operator,
                home: Home::Desktop,
                via,
            });
        };
        if matches!(
            profile.os_account,
            OsAccount::Linux { .. } | OsAccount::Windows { .. }
        ) {
            return Err(ProfileError::SessionUnavailable);
        }
        Ok(Resolved {
            id: profile.id.clone(),
            display_name: profile.display_name.clone(),
            os_account: profile.os_account.clone(),
            home: profile.home,
            via,
        })
    }

    /// Stores `bytes` as `id`'s picture: PNG, JPEG or WebP by magic, at most
    /// [`AVATAR_MAX_BYTES`]. Returns the new ETag.
    pub fn put_avatar(&self, id: &str, bytes: &[u8]) -> Result<String> {
        if bytes.len() > AVATAR_MAX_BYTES {
            bail!(
                "avatar is {} bytes, over the {AVATAR_MAX_BYTES} cap",
                bytes.len()
            );
        }
        let Some((ext, _)) = image_kind(bytes) else {
            bail!("avatar is not a PNG, JPEG or WebP image");
        };
        self.writable()?;
        let dir = self.avatar_dir(id).context("unknown profile")?;
        let path = dir.join(format!("{id}.{ext}"));
        pf_paths::replace_file(&path, bytes)
            .with_context(|| format!("write {}", path.display()))?;
        let url = format!("/api/v1/profiles/{id}/avatar");
        let set = self.mutate(|file| {
            let p = find_mut(file, id)?;
            p.avatar = Some(url.clone());
            p.updated_unix = now_unix();
            Ok(())
        });
        if let Err(e) = set {
            let _ = std::fs::remove_file(&path);
            return Err(e);
        }
        remove_avatar_files(&dir, id, Some(ext));
        Ok(etag(bytes))
    }

    pub fn avatar(&self, id: &str) -> Option<Avatar> {
        let dir = self.avatar_dir(id)?;
        AVATAR_KINDS.iter().find_map(|(ext, mime)| {
            let bytes = std::fs::read(dir.join(format!("{id}.{ext}"))).ok()?;
            Some(Avatar {
                etag: etag(&bytes),
                bytes,
                mime,
            })
        })
    }

    pub fn remove_avatar(&self, id: &str) -> Result<()> {
        let dir = self.avatar_dir(id).context("unknown profile")?;
        self.mutate(|file| {
            let p = find_mut(file, id)?;
            p.avatar = None;
            p.updated_unix = now_unix();
            Ok(())
        })?;
        remove_avatar_files(&dir, id, None);
        Ok(())
    }

    /// `profiles/` beside the file, for a stored profile whose id is safe as a file name.
    fn avatar_dir(&self, id: &str) -> Option<PathBuf> {
        let state = self.lock();
        let known = state.file.profiles.iter().any(|p| p.id == id);
        (known && safe_id(id)).then(|| state.path.with_file_name("profiles"))
    }

    /// Applies `change` to a copy, saves it, then keeps it. Refused while the file is unparsed.
    fn mutate(&self, change: impl FnOnce(&mut ProfilesFile) -> Result<()>) -> Result<()> {
        self.writable()?;
        let mut state = self.lock();
        if state.unparsed.is_some() {
            bail!(
                "{} did not parse; fix or remove it before changing profiles",
                state.path.display()
            );
        }
        let mut next = state.file.clone();
        change(&mut next)?;
        next.version = PROFILES_SCHEMA_VERSION;
        let bytes = serde_json::to_vec_pretty(&next)?;
        pf_paths::replace_secret_file(&state.path, &bytes)
            .with_context(|| format!("save {}", state.path.display()))?;
        state.legacy = legacy_index(&next);
        state.file = next;
        Ok(())
    }

    /// The box's own host writes profiles; a seat's copy is read only.
    fn writable(&self) -> Result<()> {
        if self.lock().read_only {
            bail!("profiles are the box's; a seat host never edits them");
        }
        Ok(())
    }

    /// The state, re-read first on a seat host when the box's file changed.
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.read_only {
            let now = mtime(&state.path);
            if now != state.stamp {
                let (file, unparsed) = parse(&state.path, std::fs::read(&state.path).ok());
                state.legacy = legacy_index(&file);
                state.file = file;
                state.unparsed = unparsed;
                state.stamp = now;
            }
        }
        state
    }
}

fn mtime(path: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// A missing file is empty. A planted or unparsable one is empty in memory, and an unparsable
/// one keeps its bytes so no save replaces it.
fn read(path: &Path) -> (ProfilesFile, Option<Vec<u8>>) {
    if crate::planted::quarantine_planted_secret(path) {
        return (ProfilesFile::default(), None);
    }
    parse(path, std::fs::read(path).ok())
}

fn parse(path: &Path, bytes: Option<Vec<u8>>) -> (ProfilesFile, Option<Vec<u8>>) {
    let Some(bytes) = bytes else {
        return (ProfilesFile::default(), None);
    };
    match serde_json::from_slice::<ProfilesFile>(&bytes) {
        Ok(mut file) => {
            if file.version == 0 {
                file.version = 1;
            }
            (file, None)
        }
        Err(e) => {
            tracing::error!(
                path = %path.display(),
                error = %e,
                "profiles file did not parse; profiles are read-only until it is fixed"
            );
            (ProfilesFile::default(), Some(bytes))
        }
    }
}

/// Last writer wins when a hand-edited file names one device twice.
fn legacy_index(file: &ProfilesFile) -> HashMap<String, ProfileId> {
    file.profiles
        .iter()
        .filter_map(|p| Some((p.legacy_device.as_ref()?.to_ascii_lowercase(), p.id.clone())))
        .collect()
}

/// `default_profile_id` when it names a profile, else the owner.
fn default_profile(file: &ProfilesFile) -> Option<&Profile> {
    file.default_profile_id
        .as_ref()
        .and_then(|id| file.profiles.iter().find(|p| &p.id == id))
        .or_else(|| file.profiles.iter().find(|p| is_owner(p)))
}

fn is_owner(p: &Profile) -> bool {
    p.os_account == OsAccount::Operator
}

fn find_mut<'a>(file: &'a mut ProfilesFile, id: &str) -> Result<&'a mut Profile> {
    file.profiles
        .iter_mut()
        .find(|p| p.id == id)
        .context("unknown profile")
}

/// Trimmed, controls dropped, at most [`DISPLAY_NAME_MAX`] characters. `None` when empty.
pub fn clean_name(raw: &str) -> Option<String> {
    let name: String = raw
        .chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .trim()
        .chars()
        .take(DISPLAY_NAME_MAX)
        .collect();
    let name = name.trim_end().to_string();
    (!name.is_empty()).then_some(name)
}

fn name_taken(profiles: &[Profile], name: &str, except: Option<&str>) -> bool {
    profiles
        .iter()
        .any(|p| Some(p.id.as_str()) != except && p.display_name.eq_ignore_ascii_case(name))
}

/// `#RRGGBB`, lowercased.
fn valid_accent(accent: String) -> Result<String, EditError> {
    let hex = accent.strip_prefix('#').unwrap_or("");
    if hex.len() == 6 && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(accent.to_ascii_lowercase())
    } else {
        Err(EditError::Invalid("an accent is #RRGGBB".into()))
    }
}

/// `name`, or `name 2`, `name 3`…: display names are unique ignoring case.
fn unique_name(profiles: &[Profile], name: &str) -> String {
    let taken = |n: &str| {
        profiles
            .iter()
            .any(|p| p.display_name.eq_ignore_ascii_case(n))
    };
    if !taken(name) {
        return name.to_string();
    }
    (2..)
        .map(|i| {
            let suffix = format!(" {i}");
            let keep = DISPLAY_NAME_MAX - suffix.chars().count();
            let base: String = name.chars().take(keep).collect();
            format!("{}{suffix}", base.trim_end())
        })
        .find(|n| !taken(n))
        .unwrap_or_else(|| name.to_string())
}

/// 12 hex from the name and the wall clock, as library ids are minted.
fn new_id(name: &str) -> ProfileId {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    hex::encode(&Sha256::digest(format!("{name}:{nanos}").as_bytes())[..6])
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Ids name avatar files, so a hand-edited id must not reach a path.
fn safe_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= PROFILE_ID_MAX && id.bytes().all(|b| b.is_ascii_alphanumeric())
}

const AVATAR_KINDS: [(&str, &str); 3] = [
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("webp", "image/webp"),
];

/// File extension and MIME type by magic bytes. The host never decodes the image.
fn image_kind(bytes: &[u8]) -> Option<(&'static str, &'static str)> {
    let kind = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        0
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        1
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        2
    } else {
        return None;
    };
    Some(AVATAR_KINDS[kind])
}

fn remove_avatar_files(dir: &Path, id: &str, keep: Option<&str>) {
    for (ext, _) in AVATAR_KINDS {
        if Some(ext) != keep {
            let _ = std::fs::remove_file(dir.join(format!("{id}.{ext}")));
        }
    }
}

fn etag(bytes: &[u8]) -> String {
    hex::encode(&Sha256::digest(bytes)[..8])
}

/// The owner's name from the OS: the account's full name on Unix, the console user on Windows.
fn os_display_name() -> Option<String> {
    #[cfg(unix)]
    return unix_full_name();
    #[cfg(windows)]
    return windows_console_user();
    #[cfg(not(any(unix, windows)))]
    None
}

/// The first comma field of the GECOS entry. `deck`-style accounts have none.
#[cfg(unix)]
fn unix_full_name() -> Option<String> {
    // SAFETY: an all-zero `passwd` is valid: null pointers and zero ids.
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut buf = vec![0 as libc::c_char; 4096];
    let mut out: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: `pwd`, `buf` and `out` are live for the call and `buf.len()` is its true size;
    // on success `out` points at `pwd`, whose strings point into `buf`.
    let rc = unsafe {
        libc::getpwuid_r(
            libc::getuid(),
            &mut pwd,
            buf.as_mut_ptr(),
            buf.len(),
            &mut out,
        )
    };
    if rc != 0 || out.is_null() || pwd.pw_gecos.is_null() {
        return None;
    }
    // SAFETY: `pw_gecos` is a NUL-terminated string inside `buf`, which is still alive.
    let gecos = unsafe { std::ffi::CStr::from_ptr(pwd.pw_gecos) };
    let full = gecos.to_string_lossy();
    let name = full.split(',').next()?.trim().to_string();
    (!name.is_empty()).then_some(name)
}

#[cfg(windows)]
fn windows_console_user() -> Option<String> {
    use windows::Win32::System::RemoteDesktop::{
        WTSFreeMemory, WTSGetActiveConsoleSessionId, WTSQuerySessionInformationW, WTSUserName,
        WTS_CURRENT_SERVER_HANDLE,
    };
    // SAFETY: no arguments; returns 0xFFFFFFFF when no session is attached to the console.
    let session = unsafe { WTSGetActiveConsoleSessionId() };
    if session == u32::MAX {
        return None;
    }
    let mut buf = windows::core::PWSTR::null();
    let mut len = 0_u32;
    // SAFETY: `buf` and `len` are live out-pointers; WTS allocates the string it returns.
    unsafe {
        WTSQuerySessionInformationW(
            Some(WTS_CURRENT_SERVER_HANDLE),
            session,
            WTSUserName,
            &mut buf,
            &mut len,
        )
    }
    .ok()?;
    // SAFETY: on success `buf` is a NUL-terminated string WTS allocated.
    let name = unsafe { buf.to_string() }.ok();
    // SAFETY: `buf` came from WTSQuerySessionInformationW and is freed exactly once.
    unsafe { WTSFreeMemory(buf.0.cast()) };
    name.filter(|n| !n.trim().is_empty())
}

#[cfg(test)]
mod tests;
