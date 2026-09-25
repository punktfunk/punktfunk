//! Channel and "is the manifest newer than this install?"
//!
//! Canary packaging spells one CI build many ways (`~ciN` deb, `-0.ciN` rpm,
//! `M.m.N` Windows). String compare is meaningless; canary uses `(major, minor)`
//! then the CI run number. Stable uses the plain triple.
//!
//! Unparseable pairs never flag an update. The UI may still show both strings.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    Stable,
    Canary,
}

impl Channel {
    pub fn as_str(self) -> &'static str {
        match self {
            Channel::Stable => "stable",
            Channel::Canary => "canary",
        }
    }
}

/// Leading `major.minor.patch` of a version string, ignoring any suffix (`~ci…`, `-1`, `+…`).
pub fn triple(v: &str) -> Option<(u64, u64, u64)> {
    let mut parts = v
        .split(|c: char| !c.is_ascii_digit())
        .filter(|s| !s.is_empty());
    // Non-digit split yields extra tokens (`~ci10250`). Require a leading digit or it is not a version.
    if !v.starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    Some((
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
    ))
}

/// CI run hidden in the canary string: `~ciN` / `.ciN` (deb/rpm) or patch ≥ 1000 (Windows).
/// Stable strings return `None`.
pub fn canary_run(version: &str) -> Option<u64> {
    let mut rest = version;
    while let Some(pos) = rest.find("ci") {
        let digits: String = rest[pos + 2..]
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        if !digits.is_empty() {
            return digits.parse().ok();
        }
        rest = &rest[pos + 2..];
    }
    match triple(version) {
        Some((_, _, patch)) if patch >= 1000 => Some(patch),
        _ => None,
    }
}

/// Canary compares run numbers, not patch fields; see module docs.
pub fn is_newer(
    manifest_version: &str,
    manifest_ci_run: Option<u64>,
    current: &str,
    channel: Channel,
) -> bool {
    let (Some(m), Some(c)) = (triple(manifest_version), triple(current)) else {
        return false;
    };
    match channel {
        Channel::Stable => m > c,
        Channel::Canary => {
            if (m.0, m.1) != (c.0, c.1) {
                return (m.0, m.1) > (c.0, c.1);
            }
            let manifest_run = manifest_ci_run.or_else(|| canary_run(manifest_version));
            match (manifest_run, canary_run(current)) {
                (Some(mr), Some(cr)) => mr > cr,
                _ => false,
            }
        }
    }
}

/// Windows canary is `M.m.<run>` with run ≥ 1000; stable patch stays small.
pub fn windows_channel_of(version: &str) -> Channel {
    match triple(version) {
        Some((_, _, patch)) if patch >= 1000 => Channel::Canary,
        _ => Channel::Stable,
    }
}

/// `CHANNEL=` line in the sysext updater's shell-style conf.
pub fn conf_channel(conf: &str) -> Option<Channel> {
    for line in conf.lines() {
        if let Some(v) = line.trim().strip_prefix("CHANNEL=") {
            return Some(match v.trim() {
                "canary" => Channel::Canary,
                _ => Channel::Stable,
            });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn triples() {
        assert_eq!(triple("0.23.0"), Some((0, 23, 0)));
        assert_eq!(triple("0.23.0~ci10250.gab12cd34"), Some((0, 23, 0)));
        assert_eq!(triple("0.23.10250"), Some((0, 23, 10250)));
        assert_eq!(triple("garbage"), None);
        assert_eq!(triple("1.2"), None);
    }

    #[test]
    fn canary_runs() {
        assert_eq!(canary_run("0.23.0~ci10250.gab12cd34"), Some(10250));
        assert_eq!(canary_run("0.23.0-0.ci777.g12345678"), Some(777));
        assert_eq!(canary_run("0.23.10250"), Some(10250)); // Windows/decky: run-as-patch
        assert_eq!(canary_run("0.23.0"), None);
        assert_eq!(canary_run("0.23.0-1"), None);
    }

    #[test]
    fn a_source_build_commit_suffix_is_not_a_ci_run() {
        // The Deck stamps `X.Y.Z+g<sha>` so one rebuild can be told from the next. A hex
        // sha cannot spell `ci`, so the canary axis still reads "no run number" and a
        // published run never compares against a checkout.
        assert_eq!(triple("0.35.0+gad2aee123"), Some((0, 35, 0)));
        assert_eq!(canary_run("0.35.0+gad2aee123"), None);
        // Nix stamps the same shape behind its `-nix` marker (packaging/nix/packages.nix).
        assert_eq!(triple("0.39.0-nix+g8b1c2d3e"), Some((0, 39, 0)));
        assert_eq!(canary_run("0.39.0-nix+g8b1c2d3e"), None);
        assert!(!is_newer(
            "0.35.0~ci412.gdeadbeef",
            Some(412),
            "0.35.0+gad2aee123",
            Channel::Canary
        ));
    }

    #[test]
    fn newer_stable() {
        assert!(is_newer("0.23.0", None, "0.22.2", Channel::Stable));
        assert!(!is_newer("0.22.2", None, "0.22.2", Channel::Stable));
        assert!(!is_newer("0.22.1", None, "0.22.2", Channel::Stable));
        assert!(!is_newer("not-a-version", None, "0.22.2", Channel::Stable));
    }

    #[test]
    fn newer_canary_compares_runs_not_patch() {
        // Same CI run, different spelling (Windows vs deb) is not newer; triple compare would say it is.
        assert!(!is_newer(
            "0.23.10250",
            Some(10250),
            "0.23.0~ci10250.gab12cd34",
            Channel::Canary
        ));
        assert!(is_newer(
            "0.23.10251",
            Some(10251),
            "0.23.0~ci10250.gab12cd34",
            Channel::Canary
        ));
        assert!(is_newer(
            "0.24.100",
            Some(100),
            "0.23.0~ci10250.g12",
            Channel::Canary
        ));
        // Missing run on either side: never guess.
        assert!(!is_newer("0.23.10250", None, "0.23.0", Channel::Canary));
    }

    #[test]
    fn windows_channel_heuristic() {
        assert_eq!(windows_channel_of("0.22.2"), Channel::Stable);
        assert_eq!(windows_channel_of("0.23.10118"), Channel::Canary);
    }

    #[test]
    fn conf_channels() {
        assert_eq!(conf_channel("CHANNEL=canary\n"), Some(Channel::Canary));
        assert_eq!(
            conf_channel("# a comment\nCHANNEL=stable"),
            Some(Channel::Stable)
        );
        assert_eq!(conf_channel("KEEP=6\n"), None);
    }
}
