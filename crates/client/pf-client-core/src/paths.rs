//! The XDG and System32 path rules, as the host's `pf_paths` applies them. Kept here so the
//! client does not link the host's crate.

/// `$var` when it holds an absolute path, else `$HOME/<fallback>`. The XDG base-dir spec
/// ignores an empty or relative value. `None` with neither, so a caller never picks the cwd.
#[cfg(all(desktop, target_os = "linux"))]
pub(crate) fn xdg_home(var: &str, fallback: &str) -> Option<std::path::PathBuf> {
    std::env::var_os(var)
        .map(std::path::PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(fallback)))
}

/// `%SystemRoot%\System32\<rel>`, else under `%WINDIR%`, else `C:\Windows`: never a bare
/// name, which `CreateProcess` would look up beside the exe first.
#[cfg(any(windows, test))]
pub(crate) fn system32(rel: &str) -> String {
    let root = std::env::var("SystemRoot")
        .or_else(|_| std::env::var("WINDIR"))
        .unwrap_or_else(|_| r"C:\Windows".to_string());
    format!(r"{root}\System32\{rel}")
}

#[cfg(test)]
mod tests {
    #[test]
    fn system_tools_are_named_under_system32_never_by_bare_name() {
        let p = super::system32("icacls.exe");
        assert!(p.ends_with(r"\System32\icacls.exe"), "{p}");
    }
}
