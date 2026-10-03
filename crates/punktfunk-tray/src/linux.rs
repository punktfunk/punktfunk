//! Linux StatusNotifierItem tray (`ksni`/`zbus`), fed by the status poller.
//!
//! The host is the systemd **user** unit `punktfunk-host.service`. Start/stop/restart
//! are `systemctl --user` — no polkit. KDE renders SNI natively; GNOME needs the
//! AppIndicator extension or the icon is missing. `--autostart` then exits silently
//! instead of failing every login.
//!
//! One instance per session (`flock` on `$XDG_RUNTIME_DIR/punktfunk-tray.lock`).
//! Status model and poller: `status.rs`. Service-vs-machine restart wording:
//! `design/host-actions.md`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use crate::status::{self, Poller, TrayStatus};

/// Poller writes `status` / `web_console` through `Handle::update`, which re-emits SNI props.
struct HostTray {
    status: TrayStatus,
    web_port: u16,
    /// Loopback probe of the console. Labels the always-present "Open web console" row; never hides it.
    web_console: bool,
    /// Set after `spawn` (the poller needs the tray handle first) so menu actions can `poke`.
    poller: Arc<OnceLock<Poller>>,
}

impl HostTray {
    fn systemctl(&self, verb: &str) {
        let _ = std::process::Command::new("systemctl")
            .args(["--user", verb, status::UNIT_NAME])
            .status();
        if let Some(p) = self.poller.get() {
            p.poke();
        }
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
        vec![
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
                activate: Box::new(|t: &mut Self| t.systemctl("start")),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Stop host".into(),
                visible: self.status.is_running(),
                activate: Box::new(|t: &mut Self| t.systemctl("stop")),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: status::RESTART_LABEL.into(),
                visible: self.status.can_restart(),
                activate: Box::new(|t: &mut Self| t.systemctl("restart")),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Exit tray".into(),
                activate: Box::new(|_: &mut Self| std::process::exit(0)),
                ..Default::default()
            }
            .into(),
        ]
    }

    /// Stay registered across a watcher drop (plasmashell restart, GNOME reload).
    /// `--autostart` waits when SNI was never there (`assume_sni_available` below).
    fn watcher_offline(&self, _reason: ksni::OfflineReason) -> bool {
        true
    }
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
    let _ = args.start_host; // Windows Start-menu link; the tray menu starts a Linux host
    if args.quit {
        // Windows-only convenience for the uninstaller; nothing to do here.
        return Ok(());
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
        Box::new(move |st, console_up| {
            let updated = update_handle.update(|t: &mut HostTray| {
                t.status = st;
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
