//! Linux StatusNotifierItem tray (`ksni`/`zbus`), fed by the status poller.
//!
//! The host, its web console and its plugin runner are systemd **user** units
//! (`status::UNITS`). Each gets a submenu with its state and start/stop/restart via
//! `systemctl --user` — no polkit. "Start host" and `--start-host` (the launcher)
//! start all three. KDE renders SNI natively; GNOME needs the AppIndicator extension
//! or the icon is missing. `--autostart` then exits silently instead of failing every login.
//!
//! One instance per session (`flock` on `$XDG_RUNTIME_DIR/punktfunk-tray.lock`).
//! Status model and poller: `status.rs`. Service-vs-machine restart wording:
//! `design/host-actions.md`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use crate::status::{self, Poller, TrayStatus};

/// The poller writes the state fields through `Handle::update`, which re-emits SNI props.
struct HostTray {
    status: TrayStatus,
    /// The units after the host in `status::UNITS`; empty until the first poll.
    companions: Vec<TrayStatus>,
    web_port: u16,
    /// Loopback probe of the console. Labels the always-present "Open web console" row; never hides it.
    web_console: bool,
    /// Set after `spawn` (the poller needs the tray handle first) so menu actions can `poke`.
    poller: Arc<OnceLock<Poller>>,
}

impl HostTray {
    /// Off the D-Bus thread: a console start waits on web-init, a runner stop on its
    /// finalizers. Polls again once systemd answers.
    fn systemctl(&self, verb: &str, units: &[&str]) {
        let mut cmd = std::process::Command::new("systemctl");
        cmd.arg("--user").arg(verb).args(units);
        let poller = self.poller.clone();
        std::thread::spawn(move || {
            let _ = cmd.status();
            if let Some(p) = poller.get() {
                p.poke();
            }
        });
    }

    /// Empty `path` is the dashboard.
    fn open_console(&self, path: &str) {
        let url = format!("https://127.0.0.1:{}/{path}", self.web_port);
        let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    }
}

impl ksni::Tray for HostTray {
    fn id(&self) -> String {
        "punktfunk-tray".into()
    }

    fn title(&self) -> String {
        "punktfunk host".into()
    }

    fn status(&self) -> ksni::Status {
        match &self.status {
            TrayStatus::Error(_) => ksni::Status::NeedsAttention,
            s if s.pairing_attention() => ksni::Status::NeedsAttention,
            _ => ksni::Status::Active,
        }
    }

    /// `icon_pixmap` is the `cargo run` fallback when packaged hicolor names are missing.
    fn icon_name(&self) -> String {
        match &self.status {
            TrayStatus::Running(_) if self.status.is_streaming() => {
                "punktfunk-tray-streaming".into()
            }
            TrayStatus::Running(_) => "punktfunk-tray".into(),
            TrayStatus::Starting | TrayStatus::Degraded => "punktfunk-tray-degraded".into(),
            TrayStatus::Error(_) => "punktfunk-tray-error".into(),
            TrayStatus::Stopped | TrayStatus::NotInstalled => "punktfunk-tray-stopped".into(),
        }
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        // Same dot palette as scripts/gen-tray-icons.py.
        let rgb = match &self.status {
            TrayStatus::Running(_) if self.status.is_streaming() => (0xb4, 0x4c, 0xf0), // violet
            TrayStatus::Running(_) => (0x2e, 0xcc, 0x71),                               // green
            TrayStatus::Starting | TrayStatus::Degraded => (0xf0, 0xa0, 0x30),          // amber
            TrayStatus::Error(_) => (0xe7, 0x4c, 0x3c),                                 // red
            TrayStatus::Stopped | TrayStatus::NotInstalled => (0x8a, 0x8a, 0x8a),       // gray
        };
        vec![dot_icon(22, rgb), dot_icon(48, rgb)]
    }

    // Bars that pin tray items key on this title (Noctalia 4.x), so it never changes;
    // the live status goes in the description, where the SNI spec puts detail.
    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: self.title(),
            description: self.status.headline(),
            ..Default::default()
        }
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::*;
        let release = self.status.release_label();
        let mut items = vec![
            StandardItem {
                label: self.status.headline(),
                enabled: false,
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: status::console_label(self.web_console).into(),
                activate: Box::new(|t: &mut Self| t.open_console("")),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Approve pairing request…".into(),
                visible: self.status.pairing_attention(),
                activate: Box::new(|t: &mut Self| t.open_console("pairing")),
                ..Default::default()
            }
            .into(),
            StandardItem {
                visible: release.is_some(),
                label: release.unwrap_or_default(),
                activate: Box::new(|t: &mut Self| t.open_console("displays")),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Start host".into(),
                visible: self.status.can_start(),
                activate: Box::new(|t: &mut Self| {
                    t.systemctl("start", &status::UNITS.map(|(unit, _)| unit))
                }),
                ..Default::default()
            }
            .into(),
        ];
        let states = std::iter::once(&self.status).chain(&self.companions);
        for ((unit, name), st) in status::UNITS.into_iter().zip(states) {
            items.push(service_menu(name, unit, st));
        }
        items.extend([
            MenuItem::Separator,
            StandardItem {
                label: "Exit tray".into(),
                activate: Box::new(|_: &mut Self| std::process::exit(0)),
                ..Default::default()
            }
            .into(),
        ]);
        items
    }

    /// Stay registered across a watcher drop (plasmashell restart, GNOME reload).
    /// `--autostart` waits when SNI was never there (`assume_sni_available` below).
    fn watcher_offline(&self, _reason: ksni::OfflineReason) -> bool {
        true
    }
}

/// "<name> — <state>" with start / stop / restart for one unit; hidden when not installed.
fn service_menu(name: &str, unit: &'static str, st: &TrayStatus) -> ksni::MenuItem<HostTray> {
    use ksni::menu::*;
    let action = move |label: &str, enabled: bool, verb: &'static str| -> MenuItem<HostTray> {
        StandardItem {
            label: label.into(),
            enabled,
            activate: Box::new(move |t: &mut HostTray| t.systemctl(verb, &[unit])),
            ..Default::default()
        }
        .into()
    };
    let state = match st {
        TrayStatus::NotInstalled => "not installed",
        TrayStatus::Stopped => "stopped",
        TrayStatus::Starting => "starting…",
        TrayStatus::Running(_) | TrayStatus::Degraded => "running",
        TrayStatus::Error(_) => "failed",
    };
    SubMenu {
        label: format!("{name} — {state}"),
        visible: *st != TrayStatus::NotInstalled,
        submenu: vec![
            action("Start", st.can_start(), "start"),
            action("Stop", st.is_running(), "stop"),
            action("Restart", st.can_restart(), "restart"),
        ],
        ..Default::default()
    }
    .into()
}

/// ARGB32 pixmap fallback (network byte order, SNI spec) when hicolor icons are missing.
fn dot_icon(size: i32, (r, g, b): (u8, u8, u8)) -> ksni::Icon {
    let mut data = Vec::with_capacity((size * size * 4) as usize);
    let center = (size as f32 - 1.0) / 2.0;
    let radius = size as f32 * 0.38;
    for y in 0..size {
        for x in 0..size {
            let d = ((x as f32 - center).powi(2) + (y as f32 - center).powi(2)).sqrt();
            // 1 px antialiasing ramp at the rim.
            let alpha = ((radius - d + 0.5).clamp(0.0, 1.0) * 255.0) as u8;
            data.extend_from_slice(&[alpha, r, g, b]);
        }
    }
    ksni::Icon {
        width: size,
        height: size,
        data,
    }
}

/// `--autostart` skip: the packaged autostart file is installed for every desktop user.
fn host_present() -> bool {
    if status::punktfunk_config_dir().is_some_and(|d| d.exists()) {
        return true;
    }
    std::process::Command::new("systemctl")
        .args(["--user", "--quiet", "is-enabled", status::UNIT_NAME])
        .status()
        .is_ok_and(|s| s.success())
}

/// One tray per session. The `flock` is held for the process lifetime.
fn acquire_instance_lock() -> Option<std::fs::File> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("punktfunk-tray.lock"))
        .ok()?;
    file.try_lock().ok()?;
    Some(file)
}

pub fn run(args: crate::Args) -> anyhow::Result<()> {
    if args.quit {
        // Windows-only convenience for the uninstaller; nothing to do here.
        return Ok(());
    }
    if args.start_host {
        // Before the instance lock, so a launch beside a running tray still starts them.
        // `--no-block`: the console's start would wait for the host's first files.
        let _ = std::process::Command::new("systemctl")
            .args(["--user", "start", "--no-block"])
            .args(status::UNITS.map(|(unit, _)| unit))
            .status();
    }
    if args.autostart && !host_present() {
        return Ok(());
    }
    let Some(_lock) = acquire_instance_lock() else {
        return Ok(()); // another instance already runs in this session
    };

    let poller_slot = Arc::new(OnceLock::new());
    let tray = HostTray {
        status: TrayStatus::Stopped, // placeholder until the first poll
        companions: Vec::new(),
        web_port: args.web_port,
        web_console: false, // live-probed on the first poll
        poller: poller_slot.clone(),
    };
    // Autostart races the desktop watcher: wait. A manual launch fails loudly so a
    // missing AppIndicator extension is visible.
    use ksni::blocking::TrayMethods;
    let handle = match tray.assume_sni_available(args.autostart).spawn() {
        Ok(h) => h,
        Err(e) if args.autostart => {
            eprintln!("punktfunk-tray: no StatusNotifier host ({e}); exiting");
            return Ok(());
        }
        Err(e) => anyhow::bail!(
            "no StatusNotifier tray available ({e}) — on GNOME, install the AppIndicator extension"
        ),
    };

    let dead = Arc::new(AtomicBool::new(false));
    let dead_flag = dead.clone();
    let update_handle = handle.clone();
    let poller = Poller::spawn(
        args.mgmt_addr.clone(),
        args.mgmt_port,
        args.web_port,
        Box::new(move |st, console_up, companions| {
            let updated = update_handle.update(|t: &mut HostTray| {
                t.status = st;
                t.companions = companions;
                t.web_console = console_up;
            });
            if updated.is_none() {
                dead_flag.store(true, Ordering::SeqCst);
            }
        }),
    );
    let _ = poller_slot.set(poller);

    // The SNI service runs on its own thread; park until it dies.
    while !dead.load(Ordering::SeqCst) && !handle.is_closed() {
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
    Ok(())
}
