//! Which registry keys a plugin's manifest may name, and the names `SetNamedSecurityInfoW` gives
//! them. The host grants each named key a read ACE that inherits down, as SYSTEM, so a key must be
//! one a launcher keeps its state in, and never on, below or above credentials or policy.

/// Subtrees below `Software`, with any `WOW6432Node` taken out, that hold credentials or policy.
const GUARDED: [&str; 5] = [
    r"Microsoft\Windows NT\CurrentVersion\Winlogon",
    r"Microsoft\Windows\CurrentVersion\Authentication",
    r"Microsoft\Windows\CurrentVersion\Policies",
    r"Microsoft\Cryptography",
    "Policies",
];

/// A declared key as `SetNamedSecurityInfoW` names it: `HKLM\…` once as `MACHINE\…`, and
/// `HKCU\…` as `USERS\<sid>\…` under each of `sids`, as `~` names every profile. `None` for a
/// malformed key or one [`launcher_key`] refuses.
pub(super) fn named_keys_for(declared: &str, sids: &[String]) -> Option<Vec<String>> {
    if let Some(rest) = key_under(declared, "HKLM\\") {
        return launcher_key(rest, false).then(|| vec![format!("MACHINE\\{rest}")]);
    }
    let rest = key_under(declared, "HKCU\\").filter(|rest| launcher_key(rest, true))?;
    Some(sids.iter().map(|s| format!("USERS\\{s}\\{rest}")).collect())
}

/// The path below `hive` (`HKLM\`), when `declared` is a clean key there.
fn key_under<'a>(declared: &'a str, hive: &str) -> Option<&'a str> {
    let rest = declared
        .get(..hive.len())
        .filter(|p| p.eq_ignore_ascii_case(hive))
        .map(|_| &declared[hive.len()..])?;
    let clean = !rest.is_empty()
        && rest.len() <= 255
        && !rest.chars().any(char::is_control)
        && rest
            .split('\\')
            .all(|c| !c.is_empty() && c != "." && c != ".." && !c.contains(['/', '*', '?']));
    clean.then_some(rest)
}

/// Whether `rest`, a clean key below its hive, is one a launcher keeps its state in: below
/// `Software` or its `WOW6432Node`, not `Classes` itself, and not on, below or above a
/// [`GUARDED`] subtree, since the ACE inherits down. `per_user` also takes
/// `System\GameConfigStore`, each account's list of games. Whole components, any case.
fn launcher_key(rest: &str, per_user: bool) -> bool {
    fn is(a: &str, b: &str) -> bool {
        a.eq_ignore_ascii_case(b)
    }
    let parts: Vec<&str> = rest.split('\\').collect();
    let below = match parts.as_slice() {
        [system, games, ..] if per_user && is(system, "System") && is(games, "GameConfigStore") => {
            return true;
        }
        [software, wow, below @ ..] if is(software, "Software") && is(wow, "WOW6432Node") => below,
        [software, below @ ..] if is(software, "Software") => below,
        _ => return false,
    };
    let overlaps = |guarded: &str| below.iter().zip(guarded.split('\\')).all(|(a, b)| is(a, b));
    !below.is_empty()
        && !matches!(below, [classes] if is(classes, "Classes"))
        && !GUARDED.iter().any(|g| overlaps(g))
}

#[cfg(test)]
mod tests {
    use super::named_keys_for;

    /// An `HKLM` key names itself once, an `HKCU` key one per loaded account, anything
    /// malformed nothing.
    #[test]
    fn named_keys_take_the_machine_and_each_account() {
        let sids = [
            "S-1-5-21-1-2-3-1001".to_string(),
            "S-1-12-1-4-5".to_string(),
        ];
        let named = |k| named_keys_for(k, &sids);
        assert_eq!(
            named(r"HKLM\SOFTWARE\Valve\Steam").unwrap(),
            [r"MACHINE\SOFTWARE\Valve\Steam"]
        );
        assert_eq!(
            named(r"hklm\SOFTWARE\WOW6432Node\Ubisoft").unwrap(),
            [r"MACHINE\SOFTWARE\WOW6432Node\Ubisoft"]
        );
        assert_eq!(
            named(r"HKCU\System\GameConfigStore").unwrap(),
            [
                r"USERS\S-1-5-21-1-2-3-1001\System\GameConfigStore",
                r"USERS\S-1-12-1-4-5\System\GameConfigStore",
            ]
        );
        for bad in [
            r"HKLM\",
            r"HKCU\Software\..\..\S-1-5-18",
            r"HKLM\SOFTWARE\\Valve",
            r"HKLM\SOFTWARE\..\SAM",
            "HKLM\\SOFTWARE\\a\u{7}b",
            r"HKU\S-1-5-18\Software",
            r"SOFTWARE\Valve",
        ] {
            assert!(named(bad).is_none(), "{bad}");
        }
    }

    /// A launcher's key passes in any case. Other hives, bare roots, credential and policy
    /// subtrees and every key above one are refused, on whole components only.
    #[test]
    fn only_launcher_keys_take_a_grant() {
        let sids = ["S-1-5-21-1-2-3-1001".to_string()];
        let granted = |k: &str| named_keys_for(k, &sids).is_some();
        for good in [
            r"HKLM\SOFTWARE\Valve\Steam",
            r"hklm\software\wow6432node\valve\steam",
            r"HKLM\SOFTWARE\WOW6432Node\GOG.com\Games",
            r"HKLM\SOFTWARE\WOW6432Node\Ubisoft\Launcher",
            r"HKLM\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall",
            r"HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall",
            r"HKLM\SOFTWARE\Classes\punktfunk",
            r"HKLM\SOFTWARE\PoliciesFoo",
            r"HKLM\SOFTWARE\Microsoft\CryptographyX",
            r"HKCU\SOFTWARE\Ubisoft\Launcher",
            r"HKCU\System\GameConfigStore",
            r"hkcu\system\gameconfigstore\Children",
        ] {
            assert!(granted(good), "{good}");
        }
        for bad in [
            r"HKLM\SAM\SAM\Domains\Account",
            r"HKLM\SECURITY\Policy\Secrets",
            r"HKLM\SYSTEM\CurrentControlSet\Control\Lsa",
            r"HKLM\HARDWARE\DESCRIPTION",
            r"HKLM\BCD00000000",
            r"HKLM\System\GameConfigStore",
            r"HKLM\SOFTWARE",
            r"HKLM\Software\Microsoft",
            r"HKLM\SOFTWARE\WOW6432Node",
            r"HKLM\SOFTWARE\Classes",
            r"HKLM\SOFTWARE\WOW6432Node\Classes",
            r"HKLM\SOFTWARE\Microsoft\Windows NT",
            r"hklm\software\microsoft\windows nt\currentversion",
            r"HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon",
            r"HKLM\SOFTWARE\WOW6432Node\Microsoft\Windows NT\CurrentVersion\Winlogon",
            r"HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion",
            r"HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Authentication\LogonUI",
            r"HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System",
            r"HKLM\SOFTWARE\Microsoft\Cryptography",
            r"HKLM\SOFTWARE\Policies",
            r"HKLM\SOFTWARE\WOW6432Node\Policies\Microsoft",
            r"HKCU\Software",
            r"HKCU\Software\Policies\Microsoft",
            r"HKCU\Software\Microsoft\Windows\CurrentVersion\Policies\Explorer",
            r"HKCU\Environment",
            r"HKCU\System\CurrentControlSet",
            r"HKCU\System\GameConfigStoreX",
        ] {
            assert!(!granted(bad), "{bad}");
        }
    }
}
