//! What the golden suites share: the golden file check and the Windows boxes. Unit-test
//! fixtures stay in `src/fixtures.rs`; goldens never route through it.

// Each test binary uses its own subset.
#![allow(dead_code)]

use std::path::Path;

use punktfunk_setup::platform::windows::{
    NetCategory, NetProfile, TaskState, WinFacts, WinInstall,
};

/// Compares `actual` with `tests/golden/<name>.txt`. `UPDATE_GOLDEN=1` writes it instead.
pub fn golden(name: &str, actual: &str) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(format!("{name}.txt"));
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::create_dir_all(path.parent().expect("golden dir")).expect("create golden dir");
        std::fs::write(&path, actual).expect("write golden");
        return;
    }
    let expected = std::fs::read_to_string(&path).unwrap_or_else(|_| {
        panic!("no golden for {name} — run UPDATE_GOLDEN=1 cargo test -p punktfunk-setup")
    });
    assert_eq!(
        actual, expected,
        "golden {name} changed (UPDATE_GOLDEN=1 to accept)"
    );
}

/// Windows 11, nothing installed, one private network.
pub fn fresh() -> WinFacts {
    WinFacts {
        os_build: 26200,
        arch: "x64".into(),
        installed: None,
        host_env_present: false,
        web_password_present: false,
        mgmt_bind_set: false,
        competing_hosts: vec![],
        mgmt_port_in_use: false,
        networks: vec![NetProfile {
            name: "Home".into(),
            category: NetCategory::Private,
        }],
        steam_audio_drivers: true,
        tray_autostart: false,
        vulkan_layer_registered: false,
        web_task: TaskState::Absent,
        scripting_task: TaskState::Absent,
        inno_uninstaller: false,
        client_installed: None,
    }
}

/// A host the Inno installer put down, its uninstaller still registered.
pub fn upgrade() -> WinFacts {
    WinFacts {
        installed: Some(WinInstall {
            version: Some("0.34.0".into()),
            location: Some(r"C:\Program Files\punktfunk\".into()),
        }),
        host_env_present: true,
        web_password_present: true,
        tray_autostart: true,
        vulkan_layer_registered: true,
        web_task: TaskState::Disabled,
        scripting_task: TaskState::Enabled,
        inno_uninstaller: true,
        ..fresh()
    }
}

/// [`upgrade`] with no Inno uninstaller left to retire.
pub fn upgrade_without_inno() -> WinFacts {
    WinFacts {
        inno_uninstaller: false,
        ..upgrade()
    }
}

/// [`fresh`] on a network Windows calls public.
pub fn public_network() -> WinFacts {
    WinFacts {
        networks: vec![NetProfile {
            name: "Netzwerk 2".into(),
            category: NetCategory::Public,
        }],
        ..fresh()
    }
}
