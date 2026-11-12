//! A profile's own layer of the library: the titles it hides, the sources it turns off, its game
//! overrides, art picks and session settings, in `library-profiles/<profile id>/` under the
//! library dir, each in the shape of the box's file. Nothing in it runs.
//!
//! A [`View`] is whose library a call reads or writes: the box's layer alone, or a profile's on
//! top of it, with the [`Reach`] of the host that profile plays on. A layer adds and narrows,
//! never widens: what the box hides or turns off stays so. The owner's layer is the box's. With no
//! profile in view every loader reads the box's files alone; a contract seat's own view is its
//! profile's. The box host writes the layers; a seat host reads its own from its library copy.
//! Pin: `mgmt::tests::profile_view`.

use super::*;
use std::sync::{Arc, Mutex};

/// The folder under the library dir that holds one folder per profile.
const PROFILES_DIR: &str = "library-profiles";

/// A host-minted profile id: 12 lowercase hex. Only such an id becomes a path.
pub(crate) fn is_profile_id(id: &str) -> bool {
    id.len() == 12 && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// What a host that plays the box's library lists of it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct Reach {
    /// It reads the box's library from a host of its own: one account's sources are the owner's.
    pub(crate) own_host: bool,
    /// That host follows the seat contract: one virtual screen, no Game Mode of its own.
    pub(crate) contract: bool,
    /// That host is a Linux seat: it plays as its own user and runs no plugin.
    pub(crate) linux_seat: bool,
    /// That host runs on a person's own account: no program the operator wrote runs there.
    pub(crate) own_account: bool,
}

impl Reach {
    /// This process's.
    pub(crate) fn here() -> Self {
        Self {
            own_host: pf_paths::seat::box_library_dir().is_some(),
            contract: pf_paths::seat::role().follows_contract(),
            linux_seat: linux_seat(),
            own_account: pf_paths::seat::own_account(),
        }
    }

    /// A seat's host, on a person's own account when `own_account`.
    pub(crate) fn seat(own_account: bool) -> Self {
        Self {
            own_host: true,
            contract: true,
            linux_seat: cfg!(target_os = "linux"),
            own_account,
        }
    }
}

/// Whose library a call reads or writes.
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct View {
    /// The profile whose layer sits on the box's; a checked id. `None`: the box's alone.
    layer: Option<String>,
    pub(crate) reach: Reach,
}

impl View {
    /// No profile named: this host's own. A contract seat's is its profile's.
    pub fn here() -> Self {
        Self {
            layer: seat_profile(),
            reach: Reach::here(),
        }
    }

    /// Profile `id` on a host that plays as `reach`. `owner`: the box's own session, whose layer
    /// is the box's.
    pub(crate) fn of(id: &str, owner: bool, reach: Reach) -> Self {
        Self {
            layer: (!owner && is_profile_id(id)).then(|| id.to_string()),
            reach,
        }
    }

    /// A session's on this host, as its profile resolved.
    pub(crate) fn session(profile: &crate::profiles::Resolved) -> Self {
        Self::of(&profile.id, profile.owner, Reach::here())
    }

    pub(crate) fn layer(&self) -> Option<&str> {
        self.layer.as_deref()
    }

    /// The profile's layer; `None` with no profile in view.
    pub(crate) fn own_dir(&self) -> Option<PathBuf> {
        let id = self.layer.as_ref()?;
        Some(pf_paths::seat::library_dir().join(PROFILES_DIR).join(id))
    }

    /// `name` in the profile's layer; `None` with no profile in view.
    pub(crate) fn own_file(&self, name: &str) -> Option<PathBuf> {
        self.own_dir().map(|dir| dir.join(name))
    }

    /// `name` in this view's own layer, or in the box's library dir.
    pub(crate) fn path(&self, name: &str) -> PathBuf {
        self.own_file(name)
            .unwrap_or_else(|| pf_paths::seat::library_dir().join(name))
    }

    /// The box's own view of the files, for the layer under this one.
    pub(crate) fn base(&self) -> Self {
        Self {
            layer: None,
            reach: self.reach,
        }
    }
}

/// A contract seat's own profile: the one the box's `profiles.json` in its trust dir names for
/// its seat, as [`crate::profiles::Profiles::load_box`] reads it. `None` on any other host.
fn seat_profile() -> Option<String> {
    if !pf_paths::seat::role().follows_contract() {
        return None;
    }
    let path = pf_paths::seat::trust_dir()?.join("profiles.json");
    let seat = pf_paths::seat::seat_id().ok().flatten()?;
    type Kept = Mutex<Option<(PathBuf, Arc<crate::profiles::Profiles>)>>;
    static KEPT: Kept = Mutex::new(None);
    let profiles = {
        let mut kept = KEPT.lock().unwrap_or_else(|e| e.into_inner());
        match kept.as_ref() {
            Some((p, profiles)) if *p == path => profiles.clone(),
            _ => {
                let profiles = Arc::new(crate::profiles::Profiles::load_box(
                    path.clone(),
                    Some(seat),
                ));
                *kept = Some((path, profiles.clone()));
                profiles
            }
        }
    };
    let id = profiles.pick(None, None).ok()?.id;
    is_profile_id(&id).then_some(id)
}

/// The values on a row that the profile in view (`as`) set itself.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, ToSchema)]
pub struct ProfileValues {
    /// `overrides` is the profile's: its entry for the game, or for the game's source.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    #[schema(required = false)]
    pub overrides: bool,
    /// The art slots the profile picked: `portrait`, `hero`, `logo`, `header`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    #[schema(required = false)]
    pub picks: Vec<String>,
}

/// The profile's own overrides and picks, read once per list.
#[derive(Default)]
pub(crate) struct OwnValues {
    overrides: Option<Overrides>,
    picks: BTreeMap<String, Artwork>,
}

impl OwnValues {
    /// Nothing with no profile in view.
    pub(crate) fn load(view: &View) -> Self {
        if view.layer().is_none() {
            return Self::default();
        }
        Self {
            overrides: Some(Overrides::load(view)),
            picks: own_picks(view),
        }
    }

    /// `None` when the profile set nothing for `id`.
    pub(crate) fn of(&self, id: &str) -> Option<ProfileValues> {
        let overrides = self
            .overrides
            .as_ref()
            .is_some_and(|o| o.own_entry(id).is_some());
        let picks: Vec<String> = self.picks.get(id).map_or_else(Vec::new, |art| {
            ArtKind::ALL
                .iter()
                .filter(|k| art_field(art, **k).is_some())
                .map(|k| k.name().to_string())
                .collect()
        });
        (overrides || !picks.is_empty()).then_some(ProfileValues { overrides, picks })
    }
}

/// Removes profile `id`'s layer, on the host that writes the library.
pub fn forget_profile(id: &str) {
    if pf_paths::seat::box_library_dir().is_some() || !is_profile_id(id) {
        return;
    }
    let dir = pf_paths::seat::library_dir().join(PROFILES_DIR).join(id);
    match std::fs::remove_dir_all(&dir) {
        Ok(()) => wrote(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            tracing::warn!(dir = %dir.display(), error = %e, "profile library layer not removed")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_minted_id_is_a_profile_id() {
        assert!(is_profile_id("a1b2c3d4e5f6"));
        for bad in [
            "",
            "A1B2C3D4E5F6",
            "a1b2c3d4e5f",
            "a1b2c3d4e5f60",
            "../../../etc",
            "a1b2c3/4e5f6",
            "a1b2c3d4e5g6",
            "operator",
            "a1b2c3d4e5f\0",
        ] {
            assert!(!is_profile_id(bad), "{bad:?}");
        }
    }

    /// The owner's layer is the box's, and an id that is no minted one never names a folder.
    #[test]
    fn a_view_has_a_layer_only_for_a_minted_id_that_is_not_the_owner() {
        let reach = Reach::default();
        assert_eq!(View::of("a1b2c3d4e5f6", true, reach).layer(), None);
        assert_eq!(View::of("../../x", false, reach).layer(), None);
        assert_eq!(View::of("operator", false, reach).layer(), None);
        let p = View::of("a1b2c3d4e5f6", false, reach);
        assert_eq!(p.layer(), Some("a1b2c3d4e5f6"));
        let path = p.path("library-hidden.json");
        assert!(path.ends_with("library-profiles/a1b2c3d4e5f6/library-hidden.json"));
        assert_eq!(p.base().layer(), None);
        assert!(View::default().own_file("x").is_none());
    }
}
