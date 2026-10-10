//! Which gamescope WSI Vulkan layer a session's games load: ours, the distro's, or none.

use super::*;

/// Session script does `export ENABLE_GAMESCOPE_WSI=1`, clobbering a unit `--setenv` of 0.
/// `DISABLE_GAMESCOPE_WSI` is presence-based and last in the loader, so it survives. Wrong here
/// is a game with sound and input on a black screen — Steam UI is not a Vulkan client. Keep
/// `ENABLE_GAMESCOPE_WSI=0` for a layer with no `disable_environment`.
const WSI_OFF_ENV: [(&str, &str); 2] = [
    ("DISABLE_GAMESCOPE_WSI", "1"),
    ("ENABLE_GAMESCOPE_WSI", "0"),
];

/// Same tree/rev as `punktfunk-gamescope`, own layer name — coexists with the distro's.
const OUR_WSI_LAYER_DIR_DEFAULT: &str = "/usr/lib/punktfunk/vulkan/implicit_layer.d";
const OUR_WSI_LAYER_MANIFEST_NAME: &str = "punktfunk_gamescope_wsi.json";

/// FHS default; `PUNKTFUNK_GAMESCOPE_WSI_LAYER_DIR` for a store with no `/usr` (NixOS).
fn our_wsi_layer_dir() -> String {
    std::env::var("PUNKTFUNK_GAMESCOPE_WSI_LAYER_DIR")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| OUR_WSI_LAYER_DIR_DEFAULT.to_string())
}

/// Resolve once: [`WsiPlan::resolve`] can spawn `--version` probes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum WsiPlan {
    /// Our own matching layer is installed: enable it, suppress the distro's. Games get HDR.
    Ours,
    /// No layer of ours, and the distro's version triple matches the gamescope we run, so it is
    /// probably built against the same protocol. Leave the box exactly as it is.
    DistroKept,
    /// No layer of ours, and the distro's cannot be trusted. Disable it — a mismatched layer kills
    /// every Vulkan client — and accept that no game in this session can get an HDR10 swapchain.
    DistroDisabled,
}

impl WsiPlan {
    /// Spawns up to two `gamescope --version` probes in the fallback arms, so resolve once and
    /// pass the result around rather than calling this per use site.
    pub(super) fn resolve() -> Self {
        let manifest = std::path::Path::new(&our_wsi_layer_dir()).join(OUR_WSI_LAYER_MANIFEST_NAME);
        if manifest.is_file() {
            Self::Ours
        } else if wsi_layer_matches_our_gamescope() {
            Self::DistroKept
        } else {
            Self::DistroDisabled
        }
    }

    /// Nested-client environment. Implicit-layer search belongs here, never on the compositor:
    /// gamescope's own Vulkan loads every implicit layer in its process env.
    ///
    /// An `hdr` session with a live layer also gets `DXVK_HDR=1`: DXVK and vkd3d-proton report an
    /// HDR display only under it. Never without a layer, where a game would write PQ into an SDR
    /// swapchain. A launch option set by the player is applied later and wins.
    pub(super) fn env(self, hdr: bool) -> Vec<(&'static str, String)> {
        let mut env = self.layer_env();
        if hdr && self != Self::DistroDisabled {
            env.push(("DXVK_HDR", "1".to_string()));
        }
        env
    }

    fn layer_env(self) -> Vec<(&'static str, String)> {
        match self {
            // `VK_ADD_IMPLICIT_LAYER_PATH` ADDS to the loader's implicit-layer search (loader
            // 1.3.234+), so the box's own layer directories keep working; the distro's gamescope
            // layer is then switched off by name, leaving exactly one gamescope WSI layer live — ours.
            Self::Ours => vec![
                ("VK_ADD_IMPLICIT_LAYER_PATH", our_wsi_layer_dir()),
                ("PUNKTFUNK_GAMESCOPE_WSI", "1".to_string()),
                ("DISABLE_GAMESCOPE_WSI", "1".to_string()),
                ("ENABLE_GAMESCOPE_WSI", "0".to_string()),
            ],
            Self::DistroKept => Vec::new(),
            Self::DistroDisabled => WSI_OFF_ENV
                .iter()
                .map(|(name, value)| (*name, (*value).to_string()))
                .collect(),
        }
    }

    /// Compositor process: force the distro layer off. Do not add our layer path.
    pub(super) fn compositor_env(self) -> Vec<(&'static str, String)> {
        match self {
            Self::DistroKept => Vec::new(),
            Self::Ours | Self::DistroDisabled => WSI_OFF_ENV
                .iter()
                .map(|(name, value)| (*name, (*value).to_string()))
                .collect(),
        }
    }

    /// As `systemd-run` arguments, for the transient unit.
    pub(super) fn setenv_args(self, hdr: bool) -> Vec<String> {
        self.env(hdr)
            .iter()
            .map(|(name, value)| format!("--setenv={name}={value}"))
            .collect()
    }

    /// As unit-file lines, for the box-session drop-in. Trailing newline included, so whatever the
    /// body puts after it still parses — same contract as [`SessionBind::unit_lines`].
    pub(super) fn unit_lines(self, hdr: bool) -> String {
        self.env(hdr)
            .iter()
            .map(|(name, value)| format!("Environment={name}={value}\n"))
            .collect()
    }
}

/// Fallback only: version triples equal ⇒ keep the distro layer. A guess in both directions —
/// same tag with a patched protocol keeps a killer; different tag with identical protocol loses
/// HDR. Prefer [`WsiPlan::Ours`]. Unreadable either side ⇒ leave it (fail open).
fn wsi_layer_matches_our_gamescope() -> bool {
    let ours = discovery::gamescope_version_of(std::path::Path::new(gamescope_bin()));
    let distro = discovery::gamescope_version_of(std::path::Path::new(DISTRO_GAMESCOPE_PATH));
    match (ours, distro) {
        // Same upstream triple ⇒ the layer was built from the same protocol. Keep it.
        (Some(a), Some(b)) => a == b,
        // Either side unreadable: leave the layer alone rather than degrade a box that works.
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `gamescope-session-plus` runs `export ENABLE_GAMESCOPE_WSI=1` before it launches anything,
    /// so that variable alone cannot turn the layer off. Only `DISABLE_GAMESCOPE_WSI`, which the
    /// script never mentions and which the Vulkan loader treats as an unconditional force-off,
    /// survives. Dropping it leaves a game with sound and input on a black screen. See [`WSI_OFF_ENV`].
    #[test]
    fn the_wsi_opt_out_carries_the_variable_the_session_script_cannot_clobber() {
        assert!(
            WSI_OFF_ENV.contains(&("DISABLE_GAMESCOPE_WSI", "1")),
            "the clobber-proof variable is the whole point of the opt-out"
        );

        // Both spellings reach both launch paths, and neither may lose the other.
        let args = WsiPlan::DistroDisabled.setenv_args(false);
        let lines = WsiPlan::DistroDisabled.unit_lines(false);
        for (name, value) in WSI_OFF_ENV {
            assert!(args.contains(&format!("--setenv={name}={value}")), "{name}");
            assert!(
                lines.contains(&format!("Environment={name}={value}\n")),
                "{name}"
            );
        }

        // Trailing newline: the drop-in body appends nothing after this block today, but the bind
        // lines above it rely on the same contract and the order has changed before.
        assert!(lines.ends_with('\n'));
    }

    /// Shipping our own layer means both halves happen in one session: ours is switched on AND
    /// the distro's is forced off. Enabling ours while leaving theirs live would put two gamescope
    /// WSI layers in the loader's implicit set. Assert the pair, not either half.
    #[test]
    fn our_own_layer_is_enabled_and_the_distro_one_forced_off_together() {
        let env = WsiPlan::Ours.env(false);
        let get = |k: &str| {
            env.iter()
                .find(|(name, _)| *name == k)
                .map(|(_, v)| v.clone())
                .unwrap_or_else(|| panic!("{k} missing from the Ours plan"))
        };

        assert_eq!(get("VK_ADD_IMPLICIT_LAYER_PATH"), our_wsi_layer_dir());
        assert_eq!(get("PUNKTFUNK_GAMESCOPE_WSI"), "1");
        // The clobber-proof one, for exactly the reason the test above states.
        assert_eq!(get("DISABLE_GAMESCOPE_WSI"), "1");
        assert_eq!(get("ENABLE_GAMESCOPE_WSI"), "0");

        // `DistroKept` must stay genuinely inert: it is the arm that runs on a box we decided not
        // to touch, so a stray variable there would change behaviour we promised not to change.
        assert!(WsiPlan::DistroKept.env(false).is_empty());
        assert!(WsiPlan::DistroKept.unit_lines(false).is_empty());
    }

    #[test]
    fn dxvk_hdr_follows_the_session_and_needs_a_layer() {
        let has = |env: Vec<(&'static str, String)>| {
            env.iter().any(|(k, v)| *k == "DXVK_HDR" && v == "1")
        };
        assert!(has(WsiPlan::Ours.env(true)));
        assert!(has(WsiPlan::DistroKept.env(true)));
        assert!(!has(WsiPlan::Ours.env(false)));
        assert!(!has(WsiPlan::DistroKept.env(false)));
        // No layer, no HDR10 swapchain: the game would write PQ into an SDR one.
        assert!(!has(WsiPlan::DistroDisabled.env(true)));
        assert!(WsiPlan::Ours
            .setenv_args(true)
            .contains(&"--setenv=DXVK_HDR=1".to_string()));
        assert!(WsiPlan::Ours
            .unit_lines(true)
            .contains("Environment=DXVK_HDR=1\n"));
    }

    #[test]
    fn ours_layer_does_not_load_into_the_compositor() {
        let env = WsiPlan::Ours.compositor_env();
        assert!(
            !env.iter()
                .any(|(k, _)| *k == "VK_ADD_IMPLICIT_LAYER_PATH" || *k == "PUNKTFUNK_GAMESCOPE_WSI"),
            "the compositor must not search or enable our WSI layer"
        );
        assert_eq!(
            env.iter()
                .find(|(k, _)| *k == "DISABLE_GAMESCOPE_WSI")
                .map(|(_, v)| v.as_str()),
            Some("1")
        );
    }
}
