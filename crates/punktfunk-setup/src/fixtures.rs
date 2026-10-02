//! Baseline boxes for the unit tests: each test moves the fields it is about.
//!
//! The demo presets and the golden suites keep their own literals, so retuning a preset moves
//! neither a unit test nor a golden.

use crate::facts::{Facts, Family, Firewall, Nvidia, OsRelease};
use crate::platform::windows::{TaskState, WinFacts, WinInstall};

/// A Linux box with nothing on it.
pub(crate) fn fresh_facts(id: &str, family: Family) -> Facts {
    Facts {
        os: OsRelease {
            id: id.into(),
            id_like: String::new(),
            version_id: String::new(),
            pretty: id.into(),
        },
        family,
        omarchy: id == "omarchy",
        docs_page: String::new(),
        host_punt: None,
        has_flatpak_client: false,
        rpm_group: None,
        floor: None,
        couch_box: id == "bazzite" || id == "nobara",
        graphical_seat: true,
        desktop_sessions: true,
        sunshine_active: false,
        current_channel: None,
        installed_pf: vec![],
        missing: vec!["host".into(), "web-console".into(), "plugin-runner".into()],
        host_version: None,
        has_web_server: false,
        has_omarchy_bin: false,
        has_ujust: false,
        in_input_group: false,
        in_punktfunk_group: false,
        has_input_group: true,
        nvidia: Nvidia::Absent,
        firewall: Firewall::None,
        systemd_pid1: true,
        user_manager: true,
        web_unit_present: true,
        web_password_present: false,
        web_bind: None,
        mgmt_bind: None,
        ip: Some("192.168.1.10".into()),
        user: "pf".into(),
    }
}

/// A Win11 x64 box with nothing punktfunk on it and no known network.
pub(crate) fn fresh_win() -> WinFacts {
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

/// [`fresh_win`] after a 0.34 host install under Program Files, tray autostart off.
pub(crate) fn upgrade_win() -> WinFacts {
    WinFacts {
        installed: Some(WinInstall {
            version: Some("0.34.0".into()),
            location: Some(r"C:\Program Files\punktfunk\".into()),
        }),
        host_env_present: true,
        web_password_present: true,
        vulkan_layer_registered: true,
        ..fresh_win()
    }
}
