//! Host OS identity for client-facing surfaces: a slash-separated specificity
//! chain plus a human-readable name.
//!
//! Grammar is generic → specific, `family[/like][/id]`: `windows`, `macos`,
//! `linux`, `linux/debian/ubuntu`. Clients walk most-specific-first and use the
//! first token they have art for, so the host always emits the full chain.
//! On Linux the middle token is the first `ID_LIKE` ancestor in [`FAMILIES`]
//! (`ID_LIKE` is most-similar-first per os-release) and the leaf is `ID`
//! verbatim.
//!
//! The chain rides the mDNS `os=` TXT record ([`crate::discovery`]) and the
//! mgmt API's `HostInfo.os`. `PRETTY_NAME` is REST-only — TXT stays small.

use pf_host_config::os_release::OsRelease;
use std::sync::OnceLock;

/// The host's OS identity, detected once per process (os-release is static for its lifetime).
pub struct OsInfo {
    /// `windows` | `macos` | `linux[/<family>][/<id>]` — see the module doc for the grammar.
    pub chain: String,
    /// Human-readable OS name (os-release `PRETTY_NAME`; `"Windows"`/`"macOS"` elsewhere).
    pub pretty: String,
}

pub fn detect() -> &'static OsInfo {
    static OS: OnceLock<OsInfo> = OnceLock::new();
    OS.get_or_init(|| {
        if cfg!(target_os = "windows") {
            OsInfo {
                chain: "windows".into(),
                pretty: "Windows".into(),
            }
        } else if cfg!(target_os = "macos") {
            // The host only runs on Windows/Linux; macOS is the dev-build platform.
            OsInfo {
                chain: "macos".into(),
                pretty: "macOS".into(),
            }
        } else {
            linux_os_info()
        }
    })
}

/// `ID_LIKE` ancestors kept as the chain's middle token — families clients have icon art for.
const FAMILIES: &[&str] = &[
    "debian", "ubuntu", "fedora", "rhel", "arch", "opensuse", "suse", "gentoo", "alpine", "nixos",
];

fn linux_os_info() -> OsInfo {
    os_info(pf_host_config::os_release::os_release())
}

/// Chain and name from a parsed os-release, pure so tests run off Linux.
///
/// `ID`/`ID_LIKE` are lowercased and sanitized before they feed a DNS TXT
/// record. `PRETTY_NAME` falls back `NAME` → capitalized `ID` → `"Linux"`.
fn os_info(os: &OsRelease) -> OsInfo {
    let id_tok = os.id.as_deref().and_then(sanitize_token);
    // First FAMILIES ancestor; skip a token that repeats ID (some distros write ID_LIKE=ID).
    let like_tok = os
        .id_like
        .iter()
        .filter_map(|t| sanitize_token(t))
        .find(|t| FAMILIES.contains(&t.as_str()) && Some(t) != id_tok.as_ref());

    let mut chain = String::from("linux");
    for tok in [&like_tok, &id_tok].into_iter().flatten() {
        chain.push('/');
        chain.push_str(tok);
    }

    let pretty = [&os.pretty_name, &os.name]
        .into_iter()
        .flatten()
        .map(|s| s.trim().to_string())
        .find(|s| !s.is_empty())
        .or_else(|| id_tok.as_deref().map(capitalize))
        .unwrap_or_else(|| "Linux".into());

    OsInfo { chain, pretty }
}

/// Omarchy flavour: chain leaf `/omarchy`. Not a family — `ID_LIKE=arch` already
/// routes install and `InstallKind::Pacman`.
///
/// Callers: `crate::update` (notify-only; their `omarchy update` owns pacman)
/// and `crate::diagnostics`. Read from [`detect`]; Omarchy rewrites os-release
/// on every settings upgrade, so a marker file in our package would miss it.
pub fn is_omarchy() -> bool {
    detect().chain.ends_with("/omarchy")
}

/// TXT-safe `[a-z0-9._-]`, max 32. `None` if nothing survives — garbage must not reach the advert.
fn sanitize_token(raw: &str) -> Option<String> {
    let tok: String = raw
        .to_lowercase()
        .chars()
        .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
        .take(32)
        .collect();
    (!tok.is_empty()).then_some(tok)
}

/// ASCII first-letter upper; fallback when os-release names are absent.
fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_ascii_uppercase().to_string() + chars.as_str(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(contents: &str) -> (String, String) {
        let info = os_info(&OsRelease::parse(contents));
        (info.chain, info.pretty)
    }

    #[test]
    fn ubuntu_chains_through_debian() {
        let (chain, pretty) = parsed(
            "PRETTY_NAME=\"Ubuntu 24.04.2 LTS\"\nNAME=\"Ubuntu\"\nID=ubuntu\nID_LIKE=debian\n",
        );
        assert_eq!(chain, "linux/debian/ubuntu");
        assert_eq!(pretty, "Ubuntu 24.04.2 LTS");
    }

    #[test]
    fn bazzite_quoted_id_like() {
        let (chain, pretty) =
            parsed("ID=bazzite\nID_LIKE=\"fedora\"\nPRETTY_NAME=\"Bazzite 42 (Kinoite)\"\n");
        assert_eq!(chain, "linux/fedora/bazzite");
        assert_eq!(pretty, "Bazzite 42 (Kinoite)");
    }

    #[test]
    fn omarchy_is_arch_family_and_keeps_its_leaf() {
        let (chain, pretty) = parsed(
            "NAME=\"Omarchy\"\nPRETTY_NAME=\"Omarchy\"\nID=omarchy\nID_LIKE=arch\nVERSION_ID=4.0.1\n",
        );
        assert_eq!(chain, "linux/arch/omarchy");
        assert_eq!(pretty, "Omarchy");
        assert!(chain.ends_with("/omarchy"), "is_omarchy() reads this");
        let (arch, _) = parsed("ID=arch\nPRETTY_NAME=\"Arch Linux\"\n");
        assert!(!arch.ends_with("/omarchy"));
    }

    #[test]
    fn steamos_is_arch_family() {
        let (chain, _) = parsed("ID=steamos\nID_LIKE=arch\nPRETTY_NAME=\"SteamOS\"\n");
        assert_eq!(chain, "linux/arch/steamos");
    }

    #[test]
    fn cachyos_is_arch_family() {
        let (chain, _) = parsed("ID=cachyos\nID_LIKE=\"arch\"\nNAME=\"CachyOS Linux\"\n");
        assert_eq!(chain, "linux/arch/cachyos");
    }

    #[test]
    fn nobara_takes_rhel_the_first_recognized_ancestor() {
        // ID_LIKE is most-similar-first: rhel wins over fedora.
        let (chain, pretty) = parsed(
            "NAME=\"Nobara Linux\"\nID=nobara\nID_LIKE=\"rhel centos fedora\"\nPRETTY_NAME=\"Nobara Linux 44 (KDE Plasma Desktop Edition)\"\n",
        );
        assert_eq!(chain, "linux/rhel/nobara");
        assert_eq!(pretty, "Nobara Linux 44 (KDE Plasma Desktop Edition)");
    }

    #[test]
    fn nixos_without_id_like_is_two_segments() {
        let (chain, _) = parsed("ID=nixos\nPRETTY_NAME=\"NixOS 25.05 (Warbler)\"\n");
        assert_eq!(chain, "linux/nixos");
    }

    #[test]
    fn plain_root_distro() {
        let (chain, _) = parsed("ID=arch\nNAME=\"Arch Linux\"\n");
        assert_eq!(chain, "linux/arch");
    }

    #[test]
    fn multi_ancestor_id_like_takes_first_recognized() {
        let (chain, _) = parsed("ID=\"centos\"\nID_LIKE=\"rhel fedora\"\n");
        assert_eq!(chain, "linux/rhel/centos");
    }

    #[test]
    fn pop_takes_ubuntu_over_debian() {
        let (chain, _) = parsed("ID=pop\nID_LIKE=\"ubuntu debian\"\n");
        assert_eq!(chain, "linux/ubuntu/pop");
    }

    #[test]
    fn reflexive_id_like_is_skipped() {
        let (chain, _) = parsed("ID=fedora\nID_LIKE=fedora\n");
        assert_eq!(chain, "linux/fedora");
    }

    #[test]
    fn unrecognized_id_like_is_dropped_leaf_kept() {
        let (chain, _) = parsed("ID=chimera\nID_LIKE=frontier\n");
        assert_eq!(chain, "linux/chimera");
    }

    #[test]
    fn garbage_and_empty_fall_back_to_linux() {
        assert_eq!(parsed("").0, "linux");
        assert_eq!(parsed("not an os-release file at all\n===\n").0, "linux");
        assert_eq!(parsed("").1, "Linux");
    }

    #[test]
    fn hostile_id_is_sanitized_for_txt() {
        // Quotes/spaces/slashes must not reach TXT or split the chain.
        let (chain, _) = parsed("ID=\"Ubu ntu/EVIL=1\"\n");
        assert_eq!(chain, "linux/ubuntuevil1");
    }

    #[test]
    fn overlong_id_is_capped() {
        let long = "x".repeat(80);
        let (chain, _) = parsed(&format!("ID={long}\n"));
        assert_eq!(chain, format!("linux/{}", "x".repeat(32)));
    }

    #[test]
    fn pretty_falls_back_name_then_id() {
        assert_eq!(
            parsed("ID=debian\nNAME=\"Debian GNU/Linux\"\n").1,
            "Debian GNU/Linux"
        );
        assert_eq!(parsed("ID=debian\n").1, "Debian");
    }

    #[test]
    fn single_quoted_values() {
        let (chain, pretty) = parsed("ID='opensuse-tumbleweed'\nID_LIKE='opensuse suse'\nPRETTY_NAME='openSUSE Tumbleweed'\n");
        assert_eq!(chain, "linux/opensuse/opensuse-tumbleweed");
        assert_eq!(pretty, "openSUSE Tumbleweed");
    }

    #[test]
    fn crlf_and_padding_tolerated() {
        let (chain, _) = parsed("  ID=ubuntu \r\n\tID_LIKE=debian\r\n");
        assert_eq!(chain, "linux/debian/ubuntu");
    }
}
