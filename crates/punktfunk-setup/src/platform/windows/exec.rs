//! Windows `WinPlan` executor. One walk: echo always, mutate only when not dry.
//!
//! Spawnable work goes through `CommandRunner` (FakeRunner-testable on any OS): `reg.exe` for
//! PATH and ARP, `netstat` for the port sweep (PID + port; never the localized STATE),
//! `schtasks` for tasks and the de-elevated tray. SCM stop/wait, `.lnk` writing, the env-change
//! broadcast, and Appx presence live in `sys.rs` (`cfg(windows)`; error stubs elsewhere).
//!
//! Placeholders (`<staging>`, `<temp>`, `<version>`, and the client's `%LocalAppData%`,
//! `<start menu>`, `<desktop>`) render verbatim in a dry run and are substituted from
//! `Subst` on a real one — except in the PATH edit, where `%LocalAppData%` stays literal on
//! purpose: `REG_EXPAND_SZ` expands it per user. Goldens enter through [`render`].

use std::path::{Path, PathBuf};

use super::plan::{join_argv, WinAction, WinPlan};
use super::sys;
use super::NetProbe;
use crate::exec::Failed;
use crate::plan::Level;
use crate::seam::{BasePaths, CommandRunner};
use crate::ui::Reporter;

#[derive(Debug, Clone, Default)]
pub struct Subst {
    pub version: String,
    /// Admin-only dir the driver payloads were staged into.
    pub staging: String,
    /// Same ACLs as staging; password file and generated task XML.
    pub temp: String,
    /// The per-user roots the client plan names symbolically (M4).
    pub local_app_data: String,
    pub start_menu: String,
    pub desktop: String,
}

/// `Ok` carries the files a running process kept mapped, queued to be replaced at the next
/// boot instead of failing the tree.
pub trait PayloadSource {
    fn deploy(&self, dest: &Path) -> Result<Vec<std::path::PathBuf>, String>;
}

/// Records the destinations and touches nothing.
#[derive(Debug, Default)]
pub struct FakePayload {
    pub deployed: std::cell::RefCell<Vec<String>>,
}

impl PayloadSource for FakePayload {
    fn deploy(&self, dest: &Path) -> Result<Vec<std::path::PathBuf>, String> {
        self.deployed.borrow_mut().push(dest.display().to_string());
        Ok(Vec::new())
    }
}

pub struct WinExecutor<'a> {
    pub run: &'a dyn CommandRunner,
    pub net: &'a dyn NetProbe,
    pub payload: &'a dyn PayloadSource,
    pub paths: &'a BasePaths,
    pub ui: &'a dyn Reporter,
    pub dry: bool,
    /// Silent install: no window; skip tray launch — the host's supervision starts one.
    pub silent: bool,
    /// The web password when the wizard edited it; `None` = generate at the step.
    pub web_password: Option<String>,
    pub subst: Subst,
}

/// The goldens' entry: render as `--dry-run` with seams that cannot touch anything.
pub fn render(plan: &WinPlan, ui: &dyn Reporter) {
    let run = crate::seam::FakeRunner::new();
    let net = super::FakeNet::default();
    let payload = FakePayload::default();
    let paths = BasePaths::rooted(Path::new("/nowhere"));
    WinExecutor {
        run: &run,
        net: &net,
        payload: &payload,
        paths: &paths,
        ui,
        dry: true,
        silent: false,
        web_password: None,
        subst: Subst::default(),
    }
    .execute(plan)
    .expect("a dry run cannot fail");
}

impl WinExecutor<'_> {
    pub fn execute(&self, plan: &WinPlan) -> Result<(), Failed> {
        for phase in &plan.phases {
            self.ui.say(&phase.title);
            for step in &phase.steps {
                if let Err(e) = self.step(step) {
                    self.restore_after_failure(plan);
                    return Err(e);
                }
            }
        }
        Ok(())
    }

    /// A failed upgrade owes back what its stop took: run the plan's `RestoreTasks` and its
    /// `service start` wherever they sit. A host that can't start is no worse off than one
    /// left stopped. A fresh install stopped nothing and starts nothing here.
    fn restore_after_failure(&self, plan: &WinPlan) {
        let stopped = plan
            .steps()
            .any(|s| matches!(s, WinAction::StopHostRuntime { .. }));
        for step in plan.steps() {
            let owed = match step {
                WinAction::RestoreTasks { .. } => true,
                WinAction::Run(argv) => {
                    stopped && argv.ends_with(&["service".into(), "start".into()])
                }
                _ => false,
            };
            if owed {
                let _ = self.step(step);
            }
        }
    }

    fn sub(&self, s: &str) -> String {
        if self.dry {
            return s.to_string();
        }
        s.replace("<staging>", &self.subst.staging)
            .replace("<temp>", &self.subst.temp)
            .replace("<version>", &self.subst.version)
            .replace("<start menu>", &self.subst.start_menu)
            .replace("<desktop>", &self.subst.desktop)
            .replace("%LocalAppData%", &self.subst.local_app_data)
    }

    /// `lenient`: non-zero exit and a missing binary both succeed. Absence is the goal — a
    /// tray already gone must not fail uninstall.
    fn spawn(&self, argv: &[String], lenient: bool) -> Result<(), Failed> {
        let argv: Vec<String> = argv.iter().map(|a| self.sub(a)).collect();
        self.ui.plus(&join_argv(&argv));
        if self.dry {
            return Ok(());
        }
        let args: Vec<&str> = argv[1..].iter().map(String::as_str).collect();
        match self.run.probe(&argv[0], &args) {
            Some(out) if out.ok() || lenient => {
                // The host degrades a failed driver leg to a `warning:` on stderr and exits 0;
                // the transcript is the only place that warning can reach the operator.
                if let Some(line) = out.stderr.lines().rev().find(|l| !l.trim().is_empty()) {
                    self.ui.warn(line.trim());
                }
                Ok(())
            }
            Some(out) => Err(Failed(format!(
                "'{}' exited {} — {}",
                argv[0],
                out.code,
                out.stderr.lines().last().unwrap_or("(no error output)")
            ))),
            None if lenient => {
                self.ui
                    .detail(&format!("{} — not there, nothing to do", argv[0]));
                Ok(())
            }
            None => Err(Failed(format!("'{}' did not start", argv[0]))),
        }
    }

    /// Helper spawn; the step already echoed this line.
    fn spawn_quiet(&self, argv: &[&str], lenient: bool) -> Result<(), Failed> {
        let owned: Vec<String> = argv.iter().map(|a| self.sub(a)).collect();
        if self.dry {
            return Ok(());
        }
        let args: Vec<&str> = owned[1..].iter().map(String::as_str).collect();
        match self.run.probe(&owned[0], &args) {
            Some(out) if out.ok() || lenient => Ok(()),
            Some(out) => Err(Failed(format!("'{}' exited {}", owned[0], out.code))),
            None if lenient => Ok(()),
            None => Err(Failed(format!("'{}' did not start", owned[0]))),
        }
    }

    fn step(&self, action: &WinAction) -> Result<(), Failed> {
        match action {
            WinAction::Run(argv) => self.spawn(argv, false),
            WinAction::RunLenient(argv) => self.spawn(argv, true),
            // A dry run reports it like any other step; only a real one stops here.
            WinAction::Refuse(msg) => {
                if self.dry {
                    self.ui.warn(&format!("would refuse: {msg}"));
                    return Ok(());
                }
                Err(Failed(msg.clone()))
            }
            WinAction::Note(Level::Ok, text) => {
                self.ui.ok(text);
                Ok(())
            }
            WinAction::Note(Level::Warn, text) => {
                self.ui.warn(text);
                Ok(())
            }
            WinAction::DeployFiles { dest } => {
                if self.dry {
                    self.ui.ok(&format!("would unpack the payload into {dest}"));
                    return Ok(());
                }
                let deferred = self
                    .payload
                    .deploy(Path::new(&self.sub(dest)))
                    .map_err(Failed)?;
                if deferred.is_empty() {
                    self.ui.ok(&format!("payload unpacked into {dest}"));
                } else {
                    self.ui.warn(&format!(
                        "payload unpacked into {dest}; {} in-use file(s) ({}) are replaced at the \
                         next restart",
                        deferred.len(),
                        deferred
                            .iter()
                            .map(|p| p.file_name().unwrap_or_default().to_string_lossy())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
                Ok(())
            }
            WinAction::DeleteFiles { paths } => {
                if self.dry {
                    self.ui.ok(&format!("would delete {}", paths.join(", ")));
                    return Ok(());
                }
                for path in paths {
                    // `<start menu>` and friends resolve here, not in the dry run: the
                    // transcript keeps the placeholder, the real run needs the folder.
                    let path = self.sub(path);
                    match std::fs::remove_file(&path) {
                        Ok(()) => self.ui.ok(&format!("deleted {path}")),
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                            self.ui.detail(&format!("{path} — already gone"));
                        }
                        Err(e) => self.ui.warn(&format!("couldn't delete {path}: {e}")),
                    }
                }
                Ok(())
            }
            WinAction::RemoveFiles { dir } => {
                if self.dry {
                    self.ui.ok(&format!("would remove {dir}"));
                    return Ok(());
                }
                // Best-effort: the uninstaller still lives here; a locked file must not fail
                // teardown, and must not stop the sweep either (`remove_dir_all` aborts at
                // the first one — WP3.5's VM smoke left 862 files behind that way).
                let mut locked = Vec::new();
                sweep(Path::new(dir), &mut locked);
                if locked.is_empty() {
                    self.ui.ok(&format!("removed {dir}"));
                } else {
                    let deferred = locked
                        .iter()
                        .filter(|p| super::sys::delete_on_reboot(p))
                        .count();
                    // The dir itself goes last, after its files — Windows replays the list
                    // in order at boot.
                    let dir_deferred =
                        deferred == locked.len() && super::sys::delete_on_reboot(Path::new(dir));
                    self.ui.warn(&format!(
                        "{} in-use file(s) under {dir} ({}) go with the next restart{}",
                        locked.len(),
                        locked
                            .iter()
                            .map(|p| p.file_name().unwrap_or_default().to_string_lossy())
                            .collect::<Vec<_>>()
                            .join(", "),
                        if dir_deferred {
                            ""
                        } else {
                            " - or remove the folder by hand"
                        }
                    ));
                }
                Ok(())
            }
            WinAction::PathAdd { machine, dir } => self.path_edit(*machine, dir, true),
            WinAction::PathRemove { machine, dir } => self.path_edit(*machine, dir, false),
            WinAction::ArpRegister {
                key,
                display_name,
                version,
                location,
            } => self.arp_register(key, display_name, version, location),
            WinAction::ArpRemove { key } => {
                if self.dry {
                    self.ui.ok(&format!(
                        "would remove the Add/Remove Programs entry ({key})"
                    ));
                    return Ok(());
                }
                self.spawn(&["reg", "delete", key, "/f"].map(str::to_string), true)
            }
            WinAction::Shortcut { link, target } => {
                if self.dry {
                    self.ui.ok(&format!("would create {link} → {target}"));
                    return Ok(());
                }
                match sys::create_shortcut(&self.sub(link), &self.sub(target)) {
                    Ok(()) => self.ui.ok(&format!("created {link}")),
                    Err(e) => self.ui.warn(&format!("couldn't create {link}: {e}")),
                }
                Ok(())
            }
            WinAction::MakeNetworkPrivate { network } => {
                if self.dry {
                    self.ui
                        .ok(&format!("would set network '{network}' to Private"));
                    return Ok(());
                }
                if self.net.make_private(network) {
                    self.ui.ok(&format!("network '{network}' is now Private"));
                } else {
                    self.ui.warn(&format!(
                        "couldn't change '{network}' — set it to Private in Windows Settings, or re-run and open the public firewall"
                    ));
                }
                Ok(())
            }
            WinAction::StopHostRuntime { app_dir } => self.stop_host_runtime(app_dir),
            WinAction::RestoreTasks {
                web_enabled,
                scripting_enabled,
            } => {
                if self.dry {
                    self.ui
                        .ok("would re-enable only the tasks that were enabled before the stop");
                    return Ok(());
                }
                for (task, enabled) in [
                    ("PunktfunkWeb", web_enabled),
                    ("PunktfunkScripting", scripting_enabled),
                ] {
                    if *enabled == Some(true) {
                        self.spawn_quiet(&["schtasks", "/Change", "/TN", task, "/ENABLE"], true)?;
                    }
                }
                self.ui
                    .ok("re-enabled the tasks that were enabled before the stop");
                Ok(())
            }
            WinAction::WebSetup {
                app_dir,
                fresh_password,
            } => self.web_setup(app_dir, *fresh_password),
            WinAction::RegisterScriptingTask { app_dir, start_now } => {
                self.register_scripting(app_dir, *start_now)
            }
            WinAction::LaunchTray { exe } => self.launch_tray(exe),
            WinAction::EnsureAppRuntime { arch } => self.ensure_app_runtime(arch),
            WinAction::KillPortListeners { ports } => self.kill_port_listeners(ports),
        }
    }

    /// PATH via `reg.exe` so FakeRunner pins the write; type is REG_EXPAND_SZ.
    fn path_edit(&self, machine: bool, dir: &str, add: bool) -> Result<(), Failed> {
        let scope = if machine { "machine" } else { "user" };
        if self.dry {
            if add {
                self.ui.ok(&format!("would add {dir} to the {scope} PATH"));
            } else {
                self.ui.ok(&format!(
                    "would remove {dir} from the {scope} PATH (entry-by-entry, never a substring delete)"
                ));
            }
            return Ok(());
        }
        let key = if machine {
            r"HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Environment"
        } else {
            r"HKCU\Environment"
        };
        let current = self
            .run
            .probe("reg", &["query", key, "/v", "Path"])
            .filter(|o| o.ok())
            .and_then(|o| super::parse_reg_value(&o.stdout, "Path"))
            .unwrap_or_default();
        let Some(new) = (if add {
            path_with(&current, dir)
        } else {
            path_without(&current, dir)
        }) else {
            self.ui.ok(&format!("{scope} PATH already right"));
            return Ok(());
        };
        self.spawn_quiet(
            &[
                "reg",
                "add",
                key,
                "/v",
                "Path",
                "/t",
                "REG_EXPAND_SZ",
                "/d",
                &new,
                "/f",
            ],
            false,
        )?;
        self.ui.ok(&format!("{scope} PATH updated"));
        if let Err(e) = sys::broadcast_env_change() {
            self.ui
                .detail(&format!("env-change broadcast skipped: {e}"));
        }
        Ok(())
    }

    fn arp_register(
        &self,
        key: &str,
        display_name: &str,
        version: &str,
        location: &str,
    ) -> Result<(), Failed> {
        if self.dry {
            self.ui.ok(&format!(
                "would register '{display_name}' in Add/Remove Programs ({key})"
            ));
            return Ok(());
        }
        let location = self.sub(location);
        let uninstall = format!("\"{location}\\unins000.exe\"");
        let values: [(&str, &str, String); 8] = [
            ("DisplayName", "REG_SZ", display_name.into()),
            ("DisplayVersion", "REG_SZ", self.sub(version)),
            ("Publisher", "REG_SZ", "unom".into()),
            ("InstallLocation", "REG_SZ", location.clone()),
            (
                "DisplayIcon",
                "REG_SZ",
                format!("{location}\\punktfunk.ico"),
            ),
            ("UninstallString", "REG_SZ", uninstall.clone()),
            (
                "QuietUninstallString",
                "REG_SZ",
                format!("{uninstall} /VERYSILENT /SUPPRESSMSGBOXES"),
            ),
            ("NoModify", "REG_DWORD", "1".into()),
        ];
        for (name, ty, data) in &values {
            self.spawn_quiet(
                &["reg", "add", key, "/v", name, "/t", ty, "/d", data, "/f"],
                false,
            )?;
        }
        self.ui.ok(&format!(
            "registered '{display_name}' in Add/Remove Programs"
        ));
        Ok(())
    }

    fn stop_host_runtime(&self, app_dir: &str) -> Result<(), Failed> {
        if self.dry {
            self.ui
                .ok("would stop the service, every tray, and the console/plugin tasks");
            return Ok(());
        }
        if let Err(e) = sys::stop_service_wait("PunktfunkHost") {
            self.ui.warn(&format!("service stop: {e}"));
        }
        self.spawn_quiet(&["taskkill", "/F", "/IM", "punktfunk-tray.exe"], true)?;
        for task in ["PunktfunkWeb", "PunktfunkScripting"] {
            self.spawn_quiet(&["schtasks", "/Change", "/TN", task, "/DISABLE"], true)?;
            self.spawn_quiet(&["schtasks", "/End", "/TN", task], true)?;
        }
        self.ui
            .ok("stopped the service, trays and console/plugin tasks");
        // The runner's bun binds no port and outlives its task's End, so it is killed by image
        // path; and a kill returns before the image unmaps, so re-probe until a pass finds
        // nothing — the copy that follows fails on anything still mapped.
        let under = format!("{}\\", self.sub(app_dir).trim_end_matches('\\'))
            .to_lowercase()
            .replace('\'', "''");
        let kill = format!(
            "Get-Process bun -ErrorAction SilentlyContinue | Where-Object {{ $_.Path -and \
             $_.Path.ToLower().StartsWith('{under}') }} | ForEach-Object {{ Stop-Process -Force \
             -InputObject $_; $_.Id }}"
        );
        for _ in 0..40 {
            self.kill_port_listeners(&[47992, 47993, 3000])?;
            let killed = self
                .run
                .probe(
                    "powershell",
                    &["-NoProfile", "-NonInteractive", "-Command", &kill],
                )
                .is_some_and(|o| !o.stdout.trim().is_empty());
            if !killed {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
        Ok(())
    }

    fn web_setup(&self, app_dir: &str, fresh_password: bool) -> Result<(), Failed> {
        let host_exe = format!("{app_dir}\\punktfunk-host.exe");
        if self.dry {
            let password = if fresh_password {
                r#" --password-file "<temp>\webpw.txt""#
            } else {
                ""
            };
            self.ui.plus(&format!(
                r#""{host_exe}" web setup --app-dir "{app_dir}"{password}"#
            ));
            return Ok(());
        }
        let mut argv = vec![
            host_exe,
            "web".into(),
            "setup".into(),
            "--app-dir".into(),
            app_dir.to_string(),
        ];
        let mut pw_file = None;
        if fresh_password {
            let password = match &self.web_password {
                Some(p) => p.clone(),
                None => sys::random_hex(12).map_err(Failed)?,
            };
            let file = format!("{}\\webpw.txt", self.subst.temp);
            std::fs::write(&file, format!("{password}\n"))
                .map_err(|e| Failed(format!("couldn't stage the password file: {e}")))?;
            argv.push("--password-file".into());
            argv.push(file.clone());
            pw_file = Some(file);
        }
        let outcome = self.spawn(&argv, false);
        if let Some(file) = pw_file {
            let _ = std::fs::remove_file(file);
        }
        outcome
    }

    fn register_scripting(&self, app_dir: &str, start_now: bool) -> Result<(), Failed> {
        self.ui
            .plus("schtasks /Create /TN PunktfunkScripting /XML <generated> /F");
        if !self.dry {
            let xml = scripting_task_xml(app_dir);
            let file = format!("{}\\pf-scripting-task.xml", self.subst.temp);
            std::fs::write(&file, to_utf16le_bom(&xml))
                .map_err(|e| Failed(format!("couldn't stage the task XML: {e}")))?;
            let outcome = self.spawn_quiet(
                &[
                    "schtasks",
                    "/Create",
                    "/TN",
                    "PunktfunkScripting",
                    "/XML",
                    &file,
                    "/F",
                ],
                false,
            );
            let _ = std::fs::remove_file(&file);
            outcome?;
        }
        if start_now {
            self.spawn(
                &["schtasks", "/Run", "/TN", "PunktfunkScripting"].map(str::to_string),
                true,
            )?;
        }
        Ok(())
    }

    /// De-elevate via `/IT` without `/RL`: limited interactive token from an elevated process.
    /// No COM, no PowerShell.
    fn launch_tray(&self, exe: &str) -> Result<(), Failed> {
        if self.dry {
            self.ui.ok(&format!(
                "would start the tray ({exe}) — skipped in silent installs"
            ));
            return Ok(());
        }
        if self.silent {
            self.ui
                .ok("tray launch skipped (silent install) — the host's supervision starts one");
            return Ok(());
        }
        let name = "pf-tray-launch";
        for argv in [
            vec![
                "schtasks", "/Create", "/TN", name, "/TR", exe, "/SC", "ONCE", "/ST", "00:00",
                "/IT", "/F",
            ],
            vec!["schtasks", "/Run", "/TN", name],
            vec!["schtasks", "/Delete", "/TN", name, "/F"],
        ] {
            self.spawn_quiet(&argv, true)?;
        }
        self.ui.ok(&format!("started the tray ({exe})"));
        Ok(())
    }

    fn ensure_app_runtime(&self, arch: &str) -> Result<(), Failed> {
        if self.dry {
            self.ui.ok(&format!(
                "would ensure the Windows App Runtime ({arch}; downloaded when missing — a failure warns and never aborts)"
            ));
            return Ok(());
        }
        if sys::app_runtime_present() {
            self.ui.ok("Windows App Runtime already installed");
            return Ok(());
        }
        let url =
            format!("https://aka.ms/windowsappsdk/2.2/latest/windowsappruntimeinstall-{arch}.exe");
        let file = format!("{}\\windowsappruntimeinstall.exe", self.subst.temp);
        // `latest` cannot be hash-pinned, so the elevated run is gated on Microsoft's
        // Authenticode signature instead of TLS alone.
        let signed = format!(
            "$s = Get-AuthenticodeSignature -LiteralPath '{file}'; \
             exit [int](-not ($s.Status -eq 'Valid' -and \
             $s.SignerCertificate.Subject -like '*O=Microsoft Corporation*'))"
        );
        let steps: [Vec<&str>; 3] = [
            vec!["curl", "-fsSL", "-o", &file, &url],
            vec![
                "powershell",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                &signed,
            ],
            vec![&file, "--quiet"],
        ];
        for argv in steps {
            if self.spawn_quiet(&argv, false).is_err() {
                self.ui.warn(&format!(
                    "couldn't install the Windows App Runtime — install it manually: {url}"
                ));
                return Ok(());
            }
        }
        self.ui.ok("Windows App Runtime installed");
        Ok(())
    }

    fn kill_port_listeners(&self, ports: &[u16]) -> Result<(), Failed> {
        if self.dry {
            let list = ports
                .iter()
                .map(u16::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            self.ui
                .ok(&format!("would stop anything still listening on {list}"));
            return Ok(());
        }
        let Some(out) = self
            .run
            .probe("netstat", &["-ano", "-p", "TCP"])
            .filter(|o| o.ok())
        else {
            return Ok(());
        };
        for pid in pids_listening_on(&out.stdout, ports) {
            self.spawn_quiet(&["taskkill", "/F", "/PID", &pid], true)?;
        }
        Ok(())
    }
}

/// `None` = already an entry (case-insensitive, slash-insensitive).
fn path_with(current: &str, dir: &str) -> Option<String> {
    let want = dir.trim_end_matches('\\');
    if current
        .split(';')
        .any(|e| e.trim_end_matches('\\').eq_ignore_ascii_case(want))
    {
        return None;
    }
    if current.is_empty() {
        Some(dir.to_string())
    } else {
        Some(format!("{current};{dir}"))
    }
}

/// `None` = the dir was not an entry. Rebuilds entry-by-entry, never a substring delete.
fn path_without(current: &str, dir: &str) -> Option<String> {
    let want = dir.trim_end_matches('\\');
    let kept: Vec<&str> = current
        .split(';')
        .filter(|e| !e.trim_end_matches('\\').eq_ignore_ascii_case(want))
        .collect();
    if kept.len() == current.split(';').count() {
        return None;
    }
    Some(kept.join(";"))
}

/// PID column of `netstat -ano` rows whose local address ends in `ports`. STATE is localized;
/// never read it.
fn pids_listening_on(netstat: &str, ports: &[u16]) -> Vec<String> {
    let suffixes: Vec<String> = ports.iter().map(|p| format!(":{p}")).collect();
    let mut pids: Vec<String> = vec![];
    for line in netstat.lines() {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() < 4 || cols[0] != "TCP" {
            continue;
        }
        if suffixes.iter().any(|s| cols[1].ends_with(s.as_str()))
            && let Some(pid) = cols.last()
            && pid.chars().all(|c| c.is_ascii_digit())
            && !pids.iter().any(|p| p == pid)
        {
            pids.push((*pid).to_string());
        }
    }
    pids
}

/// XML because `schtasks` flags cannot express restart backoff. Boot, LocalService, 999×/1 min,
/// battery-tolerant. No `<LogonType>`: `schtasks /XML` rejects an explicit `ServiceAccount`
/// ("value malformed or out of range"), and a bare service SID registers as one anyway —
/// exactly what `Export-ScheduledTask` prints for the cmdlet-registered task.
fn scripting_task_xml(app_dir: &str) -> String {
    let cmd = format!("{app_dir}\\scripting\\scripting-run.cmd");
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <Triggers><BootTrigger><Enabled>true</Enabled></BootTrigger></Triggers>
  <Principals><Principal id="LocalService"><UserId>S-1-5-19</UserId></Principal></Principals>
  <Settings>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <RestartOnFailure><Interval>PT1M</Interval><Count>999</Count></RestartOnFailure>
  </Settings>
  <Actions Context="LocalService"><Exec><Command>{cmd}</Command></Exec></Actions>
</Task>
"#
    )
}

/// Remove everything under `dir` that will go, then `dir` itself; what stays (locked, or a
/// dir that still holds something locked) lands in `locked`.
fn sweep(dir: &Path, locked: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            sweep(&path, locked);
            let _ = std::fs::remove_dir(&path);
        } else if std::fs::remove_file(&path).is_err() {
            locked.push(path);
        }
    }
    let _ = std::fs::remove_dir(dir);
}

fn to_utf16le_bom(text: &str) -> Vec<u8> {
    let mut bytes = vec![0xFF, 0xFE];
    for unit in text.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::super::choices::WinChoices;
    use super::super::plan::{self, Artifact};
    use super::super::{FakeNet, TaskState, WinFacts, WinInstall};
    use super::*;
    use crate::seam::FakeRunner;
    use crate::ui::Plain;

    fn fresh_facts() -> WinFacts {
        WinFacts {
            os_build: 26200,
            arch: "x64".into(),
            installed: None,
            host_env_present: false,
            web_password_present: false,
            mgmt_bind_set: false,
            competing_hosts: vec![],
            mgmt_port_in_use: false,
            networks: vec![],
            steam_audio_drivers: true,
            tray_autostart: false,
            vulkan_layer_registered: false,
            web_task: TaskState::Absent,
            scripting_task: TaskState::Absent,
            inno_uninstaller: false,
            client_installed: None,
        }
    }

    fn executor<'a>(
        run: &'a FakeRunner,
        net: &'a FakeNet,
        payload: &'a FakePayload,
        paths: &'a BasePaths,
        ui: &'a dyn Reporter,
    ) -> WinExecutor<'a> {
        WinExecutor {
            run,
            net,
            payload,
            paths,
            ui,
            dry: false,
            silent: true,
            web_password: Some("test-password".into()),
            subst: Subst {
                version: "9.9.9".into(),
                staging: r"C:\stage".into(),
                temp: std::env::temp_dir().display().to_string(),
                local_app_data: r"C:\Users\me\AppData\Local".into(),
                start_menu: r"C:\Users\me\Start Menu\Programs".into(),
                desktop: r"C:\Users\me\Desktop".into(),
            },
        }
    }

    // M4: the client plan names per-user roots symbolically (so its goldens render on every
    // OS); a real run lands the payload where the user actually lives.
    #[test]
    fn a_real_client_run_expands_the_per_user_roots() {
        let (ui, _buf) = Plain::capture();
        let run = FakeRunner::new();
        let (net, payload) = (FakeNet::default(), FakePayload::default());
        let paths = BasePaths::rooted(Path::new("/box"));
        let exec = executor(&run, &net, &payload, &paths, &ui);
        exec.step(&WinAction::DeployFiles {
            dest: r"%LocalAppData%\Programs\Punktfunk".into(),
        })
        .unwrap();
        assert_eq!(
            payload.deployed.borrow().as_slice(),
            [r"C:\Users\me\AppData\Local\Programs\Punktfunk"]
        );
        assert_eq!(
            exec.sub(r"<start menu>\Punktfunk.lnk"),
            r"C:\Users\me\Start Menu\Programs\Punktfunk.lnk"
        );
    }

    #[test]
    fn path_edit_is_containment_checked_and_entry_exact() {
        assert_eq!(path_with(r"C:\a;C:\b", r"C:\c").unwrap(), r"C:\a;C:\b;C:\c");
        assert!(path_with(r"C:\a;c:\PF\", r"C:\pf").is_none());
        // Entry-by-entry: a substring of another entry survives.
        assert_eq!(
            path_without(r"C:\pf;C:\pf-tools;C:\b", r"C:\pf").unwrap(),
            r"C:\pf-tools;C:\b"
        );
        assert!(path_without(r"C:\a", r"C:\nope").is_none());
    }

    #[test]
    fn netstat_parse_matches_ports_and_ignores_the_state_word() {
        let out = "\r\nAktive Verbindungen\r\n\r\n  Proto  Lokale Adresse  Remoteadresse  Status  PID\r\n  TCP    0.0.0.0:47992   0.0.0.0:0      ABH\u{00d6}REN  4711\r\n  TCP    127.0.0.1:9000  0.0.0.0:0      ABH\u{00d6}REN  1234\r\n  TCP    [::]:47992      [::]:0         ABH\u{00d6}REN  4711\r\n";
        assert_eq!(pids_listening_on(out, &[47992, 3000]), ["4711"]);
    }

    #[test]
    fn a_real_run_substitutes_placeholders() {
        let (ui, buf) = Plain::capture();
        let run = FakeRunner::new().answer(
            r"C:\app\punktfunk-host.exe driver install --dir C:\stage\pfvdisplay",
            0,
            "",
        );
        let (net, payload) = (FakeNet::default(), FakePayload::default());
        let paths = BasePaths::rooted(Path::new("/box"));
        let exec = executor(&run, &net, &payload, &paths, &ui);
        exec.spawn(
            &[
                r"C:\app\punktfunk-host.exe",
                "driver",
                "install",
                "--dir",
                r"<staging>\pfvdisplay",
            ]
            .map(str::to_string),
            false,
        )
        .unwrap();
        assert!(buf.borrow().contains(r"C:\stage\pfvdisplay"));
    }

    #[test]
    fn a_failed_upgrade_starts_the_service_it_stopped() {
        let start = r"C:\app\punktfunk-host.exe service start";
        let run = FakeRunner::new().answer(start, 0, "");
        let (net, payload) = (FakeNet::default(), FakePayload::default());
        let paths = BasePaths::rooted(Path::new("/box"));
        let phase = |step: WinAction| super::super::plan::WinPhase {
            title: "t".into(),
            steps: vec![step],
        };
        let fails = phase(WinAction::Run(vec!["no-such-tool".into()]));
        let starts = phase(WinAction::Run(
            start.split(' ').map(str::to_string).collect(),
        ));
        let stops = phase(WinAction::StopHostRuntime {
            app_dir: r"C:\app".into(),
        });

        let (ui, buf) = Plain::capture();
        let upgrade = WinPlan {
            phases: vec![fails.clone(), stops, starts.clone()],
        };
        let exec = executor(&run, &net, &payload, &paths, &ui);
        assert!(exec.execute(&upgrade).is_err());
        assert!(buf.borrow().contains("service start"), "{}", buf.borrow());

        // A fresh install stopped nothing, so its failure starts nothing.
        let (ui, buf) = Plain::capture();
        let fresh = WinPlan {
            phases: vec![fails, starts],
        };
        let exec = executor(&run, &net, &payload, &paths, &ui);
        assert!(exec.execute(&fresh).is_err());
        assert!(!buf.borrow().contains("service start"), "{}", buf.borrow());
    }

    #[test]
    fn a_lenient_run_tolerates_exit_codes_but_not_a_missing_binary() {
        let (ui, _buf) = Plain::capture();
        let run = FakeRunner::new().with_path("taskkill");
        let (net, payload) = (FakeNet::default(), FakePayload::default());
        let paths = BasePaths::rooted(Path::new("/box"));
        let exec = executor(&run, &net, &payload, &paths, &ui);
        // Unscripted on-PATH probe exits 1: ok lenient, fatal otherwise.
        let argv = ["taskkill", "/F", "/IM", "x.exe"].map(str::to_string);
        assert!(exec.spawn(&argv, true).is_ok());
        assert!(exec.spawn(&argv, false).is_err());
        // Missing binary: ok lenient (absence is the goal), fatal otherwise.
        let missing = ["no-such-tool"].map(str::to_string);
        assert!(exec.spawn(&missing, true).is_ok());
        assert!(exec.spawn(&missing, false).is_err());
    }

    #[test]
    fn arp_register_writes_the_frozen_uninstall_contract() {
        let (ui, _buf) = Plain::capture();
        let (net, payload) = (FakeNet::default(), FakePayload::default());
        let paths = BasePaths::rooted(Path::new("/box"));
        let mut ok = FakeRunner::new();
        for (name, ty, data) in [
            ("DisplayName", "REG_SZ", "Punktfunk Host".to_string()),
            ("DisplayVersion", "REG_SZ", "9.9.9".to_string()),
            ("Publisher", "REG_SZ", "unom".to_string()),
            ("InstallLocation", "REG_SZ", r"C:\app".to_string()),
            ("DisplayIcon", "REG_SZ", r"C:\app\punktfunk.ico".to_string()),
            (
                "UninstallString",
                "REG_SZ",
                r#""C:\app\unins000.exe""#.to_string(),
            ),
            (
                "QuietUninstallString",
                "REG_SZ",
                r#""C:\app\unins000.exe" /VERYSILENT /SUPPRESSMSGBOXES"#.to_string(),
            ),
            ("NoModify", "REG_DWORD", "1".to_string()),
        ] {
            ok = ok.answer(
                &format!(
                    "reg add {} /v {name} /t {ty} /d {data} /f",
                    super::super::HOST_ARP_KEY
                ),
                0,
                "",
            );
        }
        let exec = executor(&ok, &net, &payload, &paths, &ui);
        exec.arp_register(
            super::super::HOST_ARP_KEY,
            "Punktfunk Host",
            "<version>",
            r"C:\app",
        )
        .unwrap();
        // Unscripted `reg` fails: writes go through the runner, not around it.
        let bare = FakeRunner::new();
        let exec = executor(&bare, &net, &payload, &paths, &ui);
        assert!(exec
            .arp_register(super::super::HOST_ARP_KEY, "x", "1", r"C:\app")
            .is_err());
    }

    #[test]
    fn the_full_uninstall_plan_executes_through_the_seams() {
        let facts = WinFacts {
            installed: Some(WinInstall {
                version: Some("0.34.0".into()),
                location: Some(r"C:\Program Files\punktfunk\".into()),
            }),
            ..fresh_facts()
        };
        let choices = WinChoices::derive(&facts, Artifact::Host);
        let plan = plan::build(&facts, &choices, Artifact::Host, true);
        let (ui, _buf) = Plain::capture();
        let mut run = FakeRunner::new();
        for tool in ["taskkill", "schtasks", "netsh", "reg", "netstat"] {
            run = run.with_path(tool);
        }
        for cmd in [
            "service uninstall",
            "driver uninstall",
            "driver uninstall --gamepad",
            "driver uninstall --audio",
        ] {
            run = run.answer(
                &format!(r"C:\Program Files\punktfunk\punktfunk-host.exe {cmd}"),
                0,
                "",
            );
        }
        let (net, payload) = (FakeNet::default(), FakePayload::default());
        let paths = BasePaths::rooted(Path::new("/box"));
        let exec = executor(&run, &net, &payload, &paths, &ui);
        // Lenient legs, empty PATH, missing dir: warn, never fail.
        exec.execute(&plan).unwrap();
        assert!(
            run.ran.borrow().is_empty(),
            "nothing may go through run_shell"
        );
    }

    #[test]
    fn make_network_private_goes_through_the_net_seam() {
        let (ui, _buf) = Plain::capture();
        let run = FakeRunner::new();
        let (net, payload) = (FakeNet::default(), FakePayload::default());
        let paths = BasePaths::rooted(Path::new("/box"));
        let exec = executor(&run, &net, &payload, &paths, &ui);
        exec.step(&WinAction::MakeNetworkPrivate {
            network: "Netzwerk 2".into(),
        })
        .unwrap();
        assert_eq!(net.made_private.borrow().as_slice(), ["Netzwerk 2"]);
    }

    #[test]
    fn the_sweep_takes_the_whole_tree_with_it() {
        let tmp = tempfile::tempdir().unwrap();
        let app = tmp.path().join("app");
        std::fs::create_dir_all(app.join("web/static")).unwrap();
        std::fs::write(app.join("web/static/a.js"), b"1").unwrap();
        std::fs::write(app.join("host.exe"), b"2").unwrap();
        let mut locked = Vec::new();
        sweep(&app, &mut locked);
        assert!(locked.is_empty());
        assert!(!app.exists());
    }

    #[test]
    fn scripting_task_xml_carries_the_iss_semantics() {
        let xml = scripting_task_xml(r"C:\app");
        assert!(xml.contains(r"C:\app\scripting\scripting-run.cmd"));
        assert!(xml.contains("<UserId>S-1-5-19</UserId>"));
        // WP3.5's VM smoke: schtasks exits 1 on an explicit ServiceAccount LogonType.
        assert!(!xml.contains("LogonType"));
        assert!(xml.contains("<Count>999</Count>"));
        assert!(xml.contains("<DisallowStartIfOnBatteries>false"));
        assert_eq!(to_utf16le_bom("ab"), [0xFF, 0xFE, b'a', 0, b'b', 0]);
    }
}
