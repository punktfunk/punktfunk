//! Root helper for triggered Linux package updates. Polkit starts it for members of
//! `punktfunk-update`; each unit's `ExecStart` is a fixed verb.
//!
//! * `apply` — host (`punktfunk-update.service`)
//! * `apply-client` — client (`punktfunk-client-update.service`)
//!
//! argv is the verb only. Install kind comes from a root-owned marker, the package list
//! from the local package database, payloads from the distro's signed repos. Both verbs
//! upgrade every installed `punktfunk*` package; the verb picks which marker to read and
//! which binary the post-install gate (`--version` must exit 0) runs.
//!
//! Result JSON is `/var/lib/punktfunk/{,client-}update-result.json` (root-written,
//! world-readable). stdout/stderr go to the unit journal.
//!
//! Design: `host-update-from-web-console.md`.

// `deny` not `forbid`: the `effective_uid` declaration is the one `#[allow(unsafe_code)]` here.
#![deny(unsafe_code)]

#[cfg(target_os = "linux")]
mod linux_main {
    use pf_update_check::detect::{self, Product};
    use serde::Serialize;
    use std::path::Path;
    use std::process::Command;

    const PACMAN_OPTIN_CONF: &str = "/etc/punktfunk/update.conf";

    /// Which marker to read and which binary the post-install gate runs. Two units, two
    /// paths — one path owned by two packages is a hard conflict in deb, rpm, and pacman.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Mode {
        Host,
        Client,
    }

    impl Mode {
        fn product(self) -> Product {
            match self {
                Mode::Host => Product::Host,
                Mode::Client => Product::Client,
            }
        }

        fn gate_binary(self) -> &'static str {
            match self {
                Mode::Host => "/usr/bin/punktfunk-host",
                Mode::Client => "/usr/bin/punktfunk-client",
            }
        }

        fn result_path(self) -> &'static str {
            match self {
                Mode::Host => "/var/lib/punktfunk/update-result.json",
                Mode::Client => "/var/lib/punktfunk/client-update-result.json",
            }
        }

        fn as_str(self) -> &'static str {
            match self {
                Mode::Host => "host",
                Mode::Client => "client",
            }
        }
    }

    /// JSON the unprivileged caller reads. `changed=false` is idle, not failure;
    /// `staged=true` means rpm-ostree needs a reboot to finish.
    #[derive(Serialize)]
    struct HelperResult {
        ok: bool,
        kind: String,
        before_version: String,
        after_version: String,
        changed: bool,
        staged: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        finished_unix: u64,
    }

    fn now_unix() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    /// Kind from root-owned markers and `/run/ostree-booted`, not from argv.
    fn detect_kind(mode: Mode) -> Result<&'static str, String> {
        if Path::new(mode.product().sysext_marker()).exists() {
            return match mode {
                Mode::Host => Ok("sysext"),
                // The signed sysext feed is the host image. Running it here would replace a client-only box.
                Mode::Client => Err(
                    "this client is a sysext, and the sysext feed carries the host image only \
                     — rebuild and re-install the client image instead"
                        .to_string(),
                ),
            };
        }
        let marker_path = mode.product().marker_path();
        let marker = std::fs::read_to_string(marker_path)
            .map_err(|e| format!("no install-kind marker at {marker_path}: {e}"))?;
        match detect::parse_marker(&marker, Path::new(detect::OSTREE_BOOTED).exists()) {
            Some((kind, _)) => Ok(kind.as_str()),
            None => Err(format!(
                "install-kind marker says {:?} — no root apply leg for it",
                marker.split_whitespace().next()
            )),
        }
    }

    fn run(cmd: &mut Command, what: &str) -> Result<(), String> {
        println!("pf-update: running {cmd:?}");
        let status = cmd
            .status()
            .map_err(|e| format!("{what}: launch failed: {e}"))?;
        if !status.success() {
            return Err(format!("{what}: exited {status}"));
        }
        Ok(())
    }

    fn run_capture(cmd: &mut Command, what: &str) -> Result<String, String> {
        let out = cmd
            .output()
            .map_err(|e| format!("{what}: launch failed: {e}"))?;
        if !out.status.success() {
            return Err(format!("{what}: exited {}", out.status));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    /// Names from the local package database — a host-only box must not grow a client package.
    fn installed_packages(query: &mut Command, what: &str) -> Result<Vec<String>, String> {
        let out = run_capture(query, what)?;
        let pkgs: Vec<String> = out
            .lines()
            .map(str::trim)
            .filter(|l| l.starts_with("punktfunk"))
            .map(str::to_string)
            .collect();
        if pkgs.is_empty() {
            return Err(format!("{what}: no installed punktfunk packages found"));
        }
        Ok(pkgs)
    }

    /// Post-install: `--version` must exit 0. Package-manager success is not enough.
    fn gate_version(mode: Mode) -> Result<String, String> {
        let bin = mode.gate_binary();
        run_capture(
            Command::new(bin).arg("--version"),
            &format!("{bin} --version"),
        )
    }

    /// Per-kind apply. `Ok(true)` means the new bits wait for a reboot (rpm-ostree).
    fn apply_for_kind(kind: &str) -> Result<bool, String> {
        match kind {
            "apt" => {
                // Restrict apt-get update to our list when it exists; a full refresh is slower, not safer.
                // List-Cleanup=0: apt otherwise deletes every list the restricted sources omit.
                let ours = "/etc/apt/sources.list.d/punktfunk.list";
                let mut update = Command::new("apt-get");
                update.env("DEBIAN_FRONTEND", "noninteractive");
                if Path::new(ours).exists() {
                    update.args([
                        "update",
                        "-o",
                        &format!("Dir::Etc::sourcelist={ours}"),
                        "-o",
                        "Dir::Etc::sourceparts=-",
                        "-o",
                        "APT::Get::List-Cleanup=0",
                    ]);
                } else {
                    update.arg("update");
                }
                run(&mut update, "apt-get update")?;
                let pkgs = installed_packages(
                    Command::new("dpkg-query").args(["-W", "-f", "${Package}\n", "punktfunk*"]),
                    "dpkg-query",
                )?;
                let mut install = Command::new("apt-get");
                install
                    .env("DEBIAN_FRONTEND", "noninteractive")
                    .args(["install", "--only-upgrade", "-y"])
                    .args(&pkgs);
                run(&mut install, "apt-get install --only-upgrade")?;
                Ok(false)
            }
            "dnf" => {
                let pkgs = installed_packages(
                    Command::new("rpm").args(["-qa", "--qf", "%{NAME}\n", "punktfunk*"]),
                    "rpm -qa",
                )?;
                let mut upgrade = Command::new("dnf");
                upgrade.args(["-y", "upgrade"]).args(&pkgs);
                run(&mut upgrade, "dnf upgrade")?;
                Ok(false)
            }
            "rpm-ostree" => {
                // Layered packages re-resolve only via uninstall+install in one transaction.
                // Staged: reboot activates.
                let pkgs = installed_packages(
                    Command::new("rpm").args(["-qa", "--qf", "%{NAME}\n", "punktfunk*"]),
                    "rpm -qa",
                )?;
                run(
                    Command::new("rpm-ostree").args(["refresh-md", "--force"]),
                    "rpm-ostree refresh-md",
                )?;
                let mut update = Command::new("rpm-ostree");
                update.arg("update");
                for p in &pkgs {
                    update.args(["--uninstall", p, "--install", p]);
                }
                run(&mut update, "rpm-ostree update (re-resolve)")?;
                Ok(true)
            }
            "sysext" => {
                run(
                    Command::new("punktfunk-sysext").arg("update"),
                    "punktfunk-sysext update",
                )?;
                Ok(false)
            }
            "pacman" => {
                // Partial Arch upgrades break the box. Full `-Syu` only, and only with the root-owned opt-in.
                let optin = std::fs::read_to_string(PACMAN_OPTIN_CONF)
                    .ok()
                    .map(|c| c.lines().any(|l| l.trim() == "PACMAN_FULL_SYSUPGRADE=1"))
                    .unwrap_or(false);
                if !optin {
                    return Err(format!(
                        "pacman full-sysupgrade is not opted in — set PACMAN_FULL_SYSUPGRADE=1 \
                         in {PACMAN_OPTIN_CONF} (this runs `pacman -Syu` for the WHOLE system)"
                    ));
                }
                run(
                    Command::new("pacman").args(["-Syu", "--noconfirm"]),
                    "pacman -Syu",
                )?;
                Ok(false)
            }
            other => Err(format!("no apply leg for install kind {other}")),
        }
    }

    fn write_result(mode: Mode, result: &HelperResult) {
        let path = Path::new(mode.result_path());
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let tmp = path.with_extension("json.tmp");
        if let Ok(bytes) = serde_json::to_vec_pretty(result) {
            if std::fs::write(&tmp, &bytes).is_ok() {
                let _ = std::fs::rename(&tmp, path);
            }
        }
    }

    pub fn main() {
        let arg = std::env::args().nth(1).unwrap_or_default();
        let mode = match arg.as_str() {
            "apply" => Mode::Host,
            "apply-client" => Mode::Client,
            _ => {
                eprintln!(
                    "usage: pf-update apply | apply-client   (normally via \
                     punktfunk-update.service / punktfunk-client-update.service)"
                );
                std::process::exit(2);
            }
        };
        if effective_uid() != 0 {
            eprintln!("pf-update: must run as root (start punktfunk-update.service)");
            std::process::exit(1);
        }

        let kind = match detect_kind(mode) {
            Ok(k) => k,
            Err(e) => {
                eprintln!("pf-update: {e}");
                write_result(
                    mode,
                    &HelperResult {
                        ok: false,
                        kind: "unknown".into(),
                        before_version: String::new(),
                        after_version: String::new(),
                        changed: false,
                        staged: false,
                        error: Some(e),
                        finished_unix: now_unix(),
                    },
                );
                std::process::exit(1);
            }
        };
        println!("pf-update: {} install kind {kind}", mode.as_str());
        let before = gate_version(mode).unwrap_or_default();

        let outcome = apply_for_kind(kind).and_then(|staged| {
            // Staged rpm-ostree leaves the new binary out of /usr until reboot — skip the gate.
            let after = if staged {
                before.clone()
            } else {
                gate_version(mode)
                    .map_err(|e| format!("run-the-binary gate: {e} — the update did NOT stick"))?
            };
            Ok((staged, after))
        });

        let result = match outcome {
            Ok((staged, after)) => HelperResult {
                ok: true,
                kind: kind.into(),
                changed: staged || after != before,
                staged,
                before_version: before,
                after_version: after,
                error: None,
                finished_unix: now_unix(),
            },
            Err(e) => {
                eprintln!("pf-update: {e}");
                HelperResult {
                    ok: false,
                    kind: kind.into(),
                    before_version: before.clone(),
                    after_version: before,
                    changed: false,
                    staged: false,
                    error: Some(e),
                    finished_unix: now_unix(),
                }
            }
        };
        let ok = result.ok;
        write_result(mode, &result);
        println!(
            "pf-update: {} ({} -> {}, changed: {}, staged: {})",
            if ok { "ok" } else { "FAILED" },
            result.before_version,
            result.after_version,
            result.changed,
            result.staged,
        );
        std::process::exit(if ok { 0 } else { 1 });
    }

    // Direct `geteuid` — a libc crate is not worth it here, nor `rustix::process::geteuid()`:
    // Cargo.toml's zero-dep posture is the point. `safe fn`: no arguments, no memory, cannot
    // fail. The `unsafe extern` block is the crate's sole `unsafe_code`.
    #[allow(unsafe_code)]
    unsafe extern "C" {
        #[link_name = "geteuid"]
        safe fn effective_uid() -> u32;
    }
}

#[cfg(target_os = "linux")]
fn main() {
    linux_main::main();
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("pf-update is a Linux-only root helper");
    std::process::exit(2);
}
