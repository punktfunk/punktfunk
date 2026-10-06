use super::*;
use pf_client_core::trust::Settings;

/// The row shows the global; a host's bound preset outranks it at launch, and the
/// row has to say so or "Automatic" streams as DualSense with nothing explaining it.
#[test]
fn a_bound_preset_marks_the_row_it_overrides() {
    let mut settings = Settings {
        gamepad: "auto".into(),
        ..Settings::default()
    };
    let library = crate::library::LibraryShared::default();
    let desk = crate::model::HostRow {
        addr: "10.0.0.7".into(),
        bound_preset: Some(crate::model::PresetChip {
            id: "p1".into(),
            name: "Living room".into(),
            accent: None,
            bitrate_kbps: None,
        }),
        ..crate::model::HostRow::fixture("bb", "Desk")
    };
    let hosts = [desk];
    let ctx = Ctx {
        hosts: &hosts,
        ..Ctx::test(&mut settings, &library)
    };
    let presets = vec![("p1".to_string(), "Living room".to_string())];
    let overrides = std::collections::HashMap::from([(
        "p1".to_string(),
        SettingsOverlay {
            gamepad: Some("dualsense".into()),
            ..Default::default()
        },
    )]);
    let spec = row_spec(RowId::PadType, &ctx, &presets, &overrides);
    assert!(spec.dot, "the overridden row carries the dot");
    assert_eq!(
        spec.value.as_deref(),
        Some("Automatic"),
        "the row keeps the global, which is what the console edits"
    );
    assert_eq!(
        spec.note.as_deref(),
        Some("Preset \u{201c}Living room\u{201d} on Desk: DualSense")
    );
    let spec = row_spec(RowId::Codec, &ctx, &presets, &overrides);
    assert!(
        !spec.dot && spec.note.is_none(),
        "a row the preset leaves alone carries no marker"
    );
}

/// A preset's Aspect note names the pinned size's shape among this device's own, as the
/// row itself does.
#[test]
fn a_preset_aspect_note_labels_against_the_devices_shapes() {
    let mut settings = Settings::default();
    let library = crate::library::LibraryShared::default();
    let hosts = [crate::model::HostRow {
        bound_preset: Some(crate::model::PresetChip {
            id: "p1".into(),
            name: "Living room".into(),
            accent: None,
            bitrate_kbps: None,
        }),
        ..crate::model::HostRow::fixture("bb", "Desk")
    }];
    let phone = crate::screens::Device {
        platform: crate::platform::Platform::Android,
        screen: Some(crate::shell::DeviceScreen {
            full: (3216, 1440),
            safe: (3088, 1440),
        }),
        ..crate::screens::Device::test()
    };
    let ctx = Ctx {
        hosts: &hosts,
        device: &phone,
        ..Ctx::test(&mut settings, &library)
    };
    let presets = vec![("p1".to_string(), "Living room".to_string())];
    let overrides = std::collections::HashMap::from([(
        "p1".to_string(),
        SettingsOverlay {
            width: Some(3216),
            height: Some(1440),
            ..Default::default()
        },
    )]);
    let spec = row_spec(RowId::Aspect, &ctx, &presets, &overrides);
    assert_eq!(
        spec.note.as_deref(),
        Some("Preset \u{201c}Living room\u{201d} on Desk: Screen")
    );
}

/// Section names vs `settings_sections` in `console-vectors.json`.
#[test]
fn sections_match_the_shared_vectors() {
    let raw = include_str!("../../../../../clients/shared/console-vectors.json");
    let file: serde_json::Value =
        serde_json::from_str(raw).expect("console-vectors.json must parse");
    let want: Vec<&str> = file["settings_sections"]
        .as_array()
        .expect("settings_sections")
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    let got: Vec<&str> = TABS.iter().map(|(name, _)| *name).collect();
    assert_eq!(got, want, "the sections' names and order");
}

/// The shared setting a row edits, by its `settings-catalog.json` key. `None` for a row that
/// only arranges others (Aspect ratio), navigates, or is one platform's variant (the Mac's
/// three-way Fullscreen).
fn catalog_key(id: RowId) -> Option<&'static str> {
    Some(match id {
        RowId::StartIn => "start_in",
        RowId::AutoWake => "auto_wake",
        RowId::Fullscreen => "fullscreen_on_stream",
        RowId::BackgroundKeepAlive => "background_keep_alive",
        RowId::BackgroundTimeout => "background_timeout_minutes",
        RowId::Stats => "stats_verbosity",
        RowId::GamepadUi => "gamepad_ui_enabled",
        RowId::GamepadUiMode => "gamepad_ui_mode",
        RowId::FollowOsTheme => "follow_os_theme",
        RowId::Palette => "ui_palette",
        RowId::ReduceMotion => "reduce_motion",
        RowId::LibraryView => "library_view",
        RowId::LibrarySections => "library_sections",
        RowId::HostSort => "host_sort",
        RowId::HostGrouping => "host_grouping",
        RowId::ShowAdvanced => "show_advanced",
        RowId::AdvancedStats => "advanced_stats",
        RowId::StatsPosition => "hud_placement",
        RowId::StatsSize => "stats_scale_pct",
        RowId::ExitHint => "exit_hint",
        RowId::ReduceUiResolution => "reduce_ui_resolution",
        RowId::Resolution => "resolution",
        RowId::Refresh => "refresh_hz",
        RowId::Bitrate => "bitrate_kbps",
        RowId::VideoFit => "video_fit",
        RowId::Hdr => "hdr_enabled",
        RowId::PresentPriority => "present_priority",
        RowId::SmoothBuffer => "smooth_buffer",
        RowId::RenderScale => "render_scale",
        RowId::Codec => "codec",
        RowId::Chroma444 => "enable_444",
        RowId::TenBitSdr => "ten_bit_sdr",
        RowId::Vsync => "vsync",
        RowId::AllowVrr => "allow_vrr",
        RowId::Compositor => "compositor",
        RowId::Decoder => "decoder",
        RowId::LowLatency => "low_latency",
        RowId::SecondScreen => "second_screen",
        RowId::Audio => "audio_channels",
        RowId::Mic => "mic_enabled",
        RowId::AudioFormat => "audio_format",
        RowId::KeepHostAudio => "keep_host_audio",
        RowId::EchoCancel => "echo_cancel",
        RowId::AudioRoute => "audio_route",
        RowId::Touch => "touch_mode",
        RowId::Mouse => "mouse_mode",
        RowId::InvertScroll => "invert_scroll",
        RowId::Shortcuts => "inhibit_shortcuts",
        RowId::QuickActions => "overlay_actions",
        RowId::CursorGestures => "cursor_gestures",
        RowId::PadType => "gamepad",
        RowId::PadRumble => "pad_rumble",
        RowId::PhoneRumble => "rumble_on_phone",
        RowId::PhoneGyro => "gyro_on_phone",
        RowId::PadForward => "gamepad_forwarding",
        RowId::Pad => "forward_pad",
        RowId::SystemButtons => "system_buttons",
        RowId::GuideGesture => "guide_gesture",
        RowId::PadHaptics => "pad_haptics",
        RowId::PadSpeaker => "pad_speaker",
        RowId::Sc2Passthrough => "sc2_capture",
        RowId::DsCapture => "ds_capture",
        _ => return None,
    })
}

/// Each row that edits a shared setting carries the catalogue's label, category and tier, and
/// every catalogue entry has a row here: the console is the surface every platform shares.
#[test]
fn rows_match_the_settings_catalog() {
    let raw = include_str!("../../../../../clients/shared/settings-catalog.json");
    let file: serde_json::Value = serde_json::from_str(raw).expect("settings-catalog.json parses");
    let entries = file["settings"].as_array().expect("settings");
    let library = crate::library::LibraryShared::default();
    let mut settings = Settings::default();
    let ctx = Ctx::test(&mut settings, &library);
    let mut seen = Vec::new();
    for (tab, rows) in &TABS {
        for id in rows.iter().copied() {
            let Some(key) = catalog_key(id) else { continue };
            let entry = entries
                .iter()
                .find(|e| e["key"] == key)
                .unwrap_or_else(|| panic!("{key} ({id:?}) is not in the catalogue"));
            let spec = row_spec(id, &ctx, &[], &Default::default());
            assert_eq!(spec.label, entry["label"].as_str().unwrap(), "{key}");
            assert_eq!(
                tab.to_lowercase(),
                entry["category"].as_str().unwrap(),
                "{key}"
            );
            assert_eq!(advanced(id), entry["advanced"].as_bool().unwrap(), "{key}");
            seen.push(key);
        }
    }
    for e in entries {
        let key = e["key"].as_str().unwrap();
        assert!(seen.contains(&key), "{key} has no console row");
    }
}

/// Every row's mark is one this build ships.
#[test]
fn every_row_icon_ships() {
    let rows = (TABS.iter().flat_map(|(_, rows)| rows.iter().copied()))
        .chain([RowId::Preset(0), RowId::NewPreset]);
    for id in rows {
        let icon = row_icon(id);
        assert!(crate::icons::by_name(icon).is_some(), "{id:?}: {icon}");
    }
}

/// Every refresh rate the desktop shells can persist must have an index here.
/// `step_option` answers a missing index with 0, and `REFRESH[0]` is Automatic, so a rate
/// this table lacks is silently discarded the first time the user nudges the row. On Linux
/// both desktop shells write the same client-gtk-settings.json this screen reads, so 144,
/// 165 and 240 arrive here whether or not this table offers them.
#[test]
fn refresh_table_covers_every_rate_the_desktop_shells_write() {
    // clients/linux/src/ui_settings.rs and clients/windows/src/app/settings.rs.
    for hz in [0u32, 30, 60, 90, 120, 144, 165, 240] {
        assert!(
            REFRESH.contains(&hz),
            "{hz} Hz is offered by the desktop shells but has no index in REFRESH"
        );
    }
    // The mechanism this pins: no index means index 0, which is Automatic.
    assert_eq!(step_option(None, REFRESH.len(), 1, false), Some(0));
    assert_eq!(REFRESH[0], 0, "index 0 must stay Automatic");
}

/// Throwaway config dir: the screens read the preset catalog and the known hosts
/// straight off it, and a test must not see the developer's own. Settings go through
/// `store::file_store`, which in tests is per-thread and in memory.
///
/// Points `trust::config_dir` at a throwaway directory.
/// One `OnceLock` for the binary — a second copy races the env write.
/// A developer override is replaced, so these tests cannot see a real store.
pub(crate) fn fake_home() {
    use std::sync::OnceLock;
    static HOME: OnceLock<std::path::PathBuf> = OnceLock::new();
    HOME.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("pf-settings-test-{}", std::process::id()));
        let cfg = if cfg!(windows) {
            dir.join("punktfunk")
        } else {
            dir.join(".config/punktfunk")
        };
        std::fs::create_dir_all(&cfg).unwrap();
        // SAFETY: runs at most once, inside `get_or_init` — concurrent `fake_home`
        // callers block until it returns, and nothing else in this binary mutates
        // this variable.
        unsafe { std::env::set_var("PUNKTFUNK_CONFIG_DIR", &cfg) };
        dir
    });
}

/// Draw once so hit-testing reads real strip/list geometry: 1000 wide, the strip.
fn rendered(screen: &mut SettingsScreen) -> f64 {
    rendered_w(screen, 1000)
}

/// Draw once at `w` × 800.
fn rendered_w(screen: &mut SettingsScreen, w: i32) -> f64 {
    let fonts = crate::theme::build_fonts().unwrap();
    let h = 800i32;
    let mut surface = skia_safe::surfaces::raster_n32_premul((w, h)).unwrap();
    let mut settings = Settings::default();
    let library = crate::library::LibraryShared::default();
    let mut ctx = Ctx::test(&mut settings, &library);
    let k = f64::from(h) / 800.0;
    let rect = Rect::from_ltrb(0.0, 64.0, w as f32, h as f32 - 86.0);
    let dt = 1.0 / 60.0;
    screen.render(surface.canvas(), rect, k, dt, &fonts, &mut ctx);
    screen.render_pinned(surface.canvas(), rect, k, dt, &fonts, &ctx);
    k
}

/// The tab that lists `id`.
fn tab_of(id: RowId) -> usize {
    TABS.iter()
        .position(|(_, rows)| rows.contains(&id))
        .expect("every row has a tab")
}

fn press(r: Rect) -> Pointer {
    Pointer {
        x: f64::from(r.center_x()),
        y: f64::from(r.center_y()),
        kind: crate::pointer::PointerKind::Press,
    }
}

fn with_ctx(f: impl FnOnce(&mut Ctx)) {
    let mut settings = Settings::default();
    let library = crate::library::LibraryShared::default();
    let mut ctx = Ctx::test(&mut settings, &library);
    f(&mut ctx);
}

/// Default-on where the GPU is the weak part; a stored value always wins over
/// the platform default, in both directions.
#[test]
fn reduce_ui_res_defaults_on_for_tvs_and_stays_revertible() {
    use crate::platform::Platform;
    let s = Settings::default();
    assert!(reduce_ui_res(&s, Platform::WebOS, true));
    assert!(reduce_ui_res(&s, Platform::WebOS, false));
    assert!(reduce_ui_res(&s, Platform::Android, false));
    assert!(!reduce_ui_res(&s, Platform::Android, true));
    assert!(reduce_ui_res(&s, Platform::Tizen, false));
    assert!(!reduce_ui_res(&s, Platform::Web, false));
    assert!(!reduce_ui_res(&s, Platform::Desktop, false));

    let mut off = s.clone();
    off.extra.insert(
        "webos.reduce_ui_resolution".into(),
        serde_json::Value::Bool(false),
    );
    assert!(!reduce_ui_res(&off, Platform::WebOS, true));
    let mut on = s;
    on.extra.insert(
        "android.reduce_ui_resolution".into(),
        serde_json::Value::Bool(true),
    );
    assert!(reduce_ui_res(&on, Platform::Android, true));
}

/// Each TV client owns its key: stepping the row on webOS writes `webos.*`,
/// the same place that client's store and the shell's backdrop read.
#[test]
fn the_row_writes_the_platforms_own_key() {
    let mut settings = Settings::default();
    let library = crate::library::LibraryShared::default();
    let webos = crate::screens::Device {
        platform: crate::platform::Platform::WebOS,
        ..crate::screens::Device::test()
    };
    let mut c = Ctx {
        device: &webos,
        ..Ctx::test(&mut settings, &library)
    };
    {
        let ctx = &mut c;
        assert!(adjust(RowId::ReduceUiResolution, -1, false, ctx));
        assert_eq!(
            ctx.settings
                .extra
                .get("webos.reduce_ui_resolution")
                .and_then(|v| v.as_bool()),
            Some(false)
        );
        assert!(!ctx
            .settings
            .extra
            .contains_key("android.reduce_ui_resolution"));
    }
    // A phone defaults off, so stepping right writes an explicit On.
    let phone = crate::screens::Device {
        platform: crate::platform::Platform::Android,
        fallback_ui: true,
        ..crate::screens::Device::test()
    };
    let mut c = Ctx {
        device: &phone,
        ..c
    };
    {
        let ctx = &mut c;
        assert!(adjust(RowId::ReduceUiResolution, 1, false, ctx));
        assert_eq!(
            ctx.settings
                .extra
                .get("android.reduce_ui_resolution")
                .and_then(|v| v.as_bool()),
            Some(true)
        );
    }
}

#[test]
fn a_press_on_a_pill_selects_that_tab() {
    let mut s = SettingsScreen::with_presets(Vec::new());
    rendered(&mut s);
    assert_eq!(s.tab, 0);
    for target in [3, 1, TABS.len() - 1, 0] {
        let pill = s.strip.pill(target).expect("the strip drew every pill");
        with_ctx(|ctx| {
            let mut fx = Outbox::default();
            assert!(s.pointer(press(pill), ctx, &mut fx), "the pill took it");
        });
        assert_eq!(s.tab, target, "pressing pill {target} selects it");
        // Selecting a tab re-lays the strip; re-render so the next pick is current.
        rendered(&mut s);
    }
}

#[test]
fn a_pressed_tab_restores_that_tabs_cursor() {
    let mut s = SettingsScreen::with_presets(Vec::new());
    rendered(&mut s);
    s.list.cursor = 2;
    let second = s.strip.pill(1).unwrap();
    with_ctx(|ctx| {
        let mut fx = Outbox::default();
        s.pointer(press(second), ctx, &mut fx);
    });
    assert_eq!(s.list.cursor, 0, "a fresh tab starts at its own top");
    rendered(&mut s);
    let first = s.strip.pill(0).unwrap();
    with_ctx(|ctx| {
        let mut fx = Outbox::default();
        s.pointer(press(first), ctx, &mut fx);
    });
    assert_eq!(s.list.cursor, 2, "coming back lands where it was left");
}

#[test]
fn a_press_on_a_row_focuses_and_cycles_it() {
    fake_home();
    let mut s = SettingsScreen::with_presets(Vec::new());
    s.tab = tab_of(RowId::Aspect);
    rendered(&mut s);
    let first = s.list.row_rect(0).expect("the list drew its rows");
    let mut settings = Settings::default();
    crate::store::file_store().save(&settings); // `apply_row` rebases on the store
    let library = crate::library::LibraryShared::default();
    let mut ctx = Ctx::test(&mut settings, &library);
    assert_eq!(s.row_ids(&ctx)[0], RowId::Aspect);
    let mut fx = Outbox::default();
    assert_eq!(ctx.settings.width, 0);
    assert!(s.pointer(press(first), &mut ctx, &mut fx));
    assert_eq!(s.list.cursor, 0, "the pressed row takes focus");
    assert_eq!(
        (ctx.settings.width, ctx.settings.height),
        (1920, 1200),
        "one press both focuses the row and cycles its value"
    );
}

#[test]
fn a_press_on_empty_space_is_not_consumed() {
    let mut s = SettingsScreen::with_presets(Vec::new());
    rendered(&mut s);
    with_ctx(|ctx| {
        let mut fx = Outbox::default();
        let p = Pointer {
            x: 4.0,
            y: 780.0,
            kind: crate::pointer::PointerKind::Press,
        };
        assert!(!s.pointer(p, ctx, &mut fx));
    });
}

/// Speaker row: stored `"mix"` reads Off; a step writes only `"pad"` / `"off"`.
#[test]
fn controller_audio_rows_follow_forwarding_and_speak_the_gtk_dialect() {
    let mut settings = Settings::default();
    let library = crate::library::LibraryShared::default();
    let mut ctx = Ctx::test(&mut settings, &library);
    assert!(ctx.settings.pad_haptics);
    assert_eq!(ctx.settings.pad_speaker, "pad");
    assert!(adjust(RowId::PadHaptics, 1, true, &mut ctx));
    assert!(!ctx.settings.pad_haptics);
    assert!(adjust(RowId::PadSpeaker, 1, true, &mut ctx));
    assert_eq!(ctx.settings.pad_speaker, "off");
    assert!(adjust(RowId::PadSpeaker, 1, true, &mut ctx));
    assert_eq!(ctx.settings.pad_speaker, "pad");
    ctx.settings.pad_speaker = "mix".into();
    assert!(adjust(RowId::PadSpeaker, 1, true, &mut ctx));
    assert_eq!(ctx.settings.pad_speaker, "pad");
    ctx.settings.gamepad_forwarding = false;
    assert!(!adjust(RowId::PadHaptics, 1, true, &mut ctx));
    assert!(!adjust(RowId::PadSpeaker, 1, true, &mut ctx));
}

#[test]
fn adjust_clamps_and_activate_wraps() {
    let mut settings = Settings::default();
    let library = crate::library::LibraryShared::default();
    let mut ctx = Ctx::test(&mut settings, &library);
    // Native (index 0): left refuses; right is Match window, then sizes.
    assert!(!adjust(RowId::Resolution, -1, false, &mut ctx));
    assert!(adjust(RowId::Resolution, 1, false, &mut ctx));
    assert!(ctx.settings.match_window, "Native → Match window");
    assert_eq!((ctx.settings.width, ctx.settings.height), (0, 0));
    assert!(adjust(RowId::Resolution, 1, false, &mut ctx));
    assert!(
        !ctx.settings.match_window,
        "explicit size clears the policy"
    );
    assert_eq!((ctx.settings.width, ctx.settings.height), (1280, 720));
    assert!(adjust(RowId::Resolution, -1, false, &mut ctx));
    assert!(ctx.settings.match_window);
    assert!(adjust(RowId::Resolution, -1, false, &mut ctx));
    assert!(!ctx.settings.match_window);
    assert_eq!(ctx.settings.width, 0, "back to Native");
    (ctx.settings.width, ctx.settings.height) = (5120, 2880);
    assert!(adjust(RowId::Resolution, 1, true, &mut ctx));
    assert_eq!(ctx.settings.width, 0, "wrapped to Native");
    assert!(!ctx.settings.match_window);
}

/// Android's safe-area mode is its own slot after Native: shown by name, and a nudge
/// moves off it by one step instead of snapping to Native. A phone has no window, so no
/// Match window entry sits between it and the sizes.
#[test]
fn android_resolution_row_carries_the_safe_area_mode() {
    let mut settings = Settings::default();
    settings
        .extra
        .insert(device_keys::SAFE_AREA_MODE.into(), true.into());
    let library = crate::library::LibraryShared::default();
    let device = crate::screens::Device {
        platform: crate::platform::Platform::Android,
        fallback_ui: true,
        ..crate::screens::Device::test()
    };
    let mut ctx = Ctx {
        device: &device,
        ..Ctx::test(&mut settings, &library)
    };
    let value = |ctx: &Ctx| row_spec(RowId::Resolution, ctx, &[], &Default::default()).value;
    let safe = |ctx: &Ctx| extra_bool(ctx.settings, device_keys::SAFE_AREA_MODE, false);
    assert_eq!(value(&ctx).as_deref(), Some("Native (safe area)"));
    assert!(adjust(RowId::Resolution, 1, false, &mut ctx));
    assert!(!ctx.settings.match_window, "no window to match");
    assert_eq!((ctx.settings.width, ctx.settings.height), (1280, 720));
    assert!(!safe(&ctx));
    assert!(adjust(RowId::Resolution, -1, false, &mut ctx));
    assert!(
        safe(&ctx) && !ctx.settings.match_window,
        "back to safe area"
    );
    assert!(adjust(RowId::Resolution, -1, false, &mut ctx));
    assert_eq!((ctx.settings.width, safe(&ctx)), (0, false), "Native");
    assert_eq!(value(&ctx).as_deref(), Some("Native"));
    set_extra_bool(ctx.settings, device_keys::SAFE_AREA_MODE, true);
    (ctx.settings.width, ctx.settings.height) = (1280, 720);
    assert_eq!(value(&ctx).as_deref(), Some("1280 × 720"), "a size wins");
}

/// The Aspect row moves between families at the nearest height; the
/// Resolution row then steps inside that family only.
#[test]
fn aspect_row_switches_family_at_nearest_height() {
    let mut settings = Settings::default();
    let library = crate::library::LibraryShared::default();
    let mut ctx = Ctx::test(&mut settings, &library);
    let size = |ctx: &Ctx| (ctx.settings.width, ctx.settings.height);
    assert_eq!(
        row_spec(RowId::Aspect, &ctx, &[], &Default::default())
            .value
            .as_deref(),
        Some("16:9"),
        "Native lists 16:9"
    );
    assert!(!adjust(RowId::Aspect, -1, false, &mut ctx), "16:9 is first");
    assert!(adjust(RowId::Aspect, 1, false, &mut ctx));
    assert_eq!(size(&ctx), (1920, 1200), "Native → 16:10 nearest 1080");
    (ctx.settings.width, ctx.settings.height) = (2560, 1600);
    assert!(adjust(RowId::Aspect, 1, false, &mut ctx));
    assert_eq!(size(&ctx), (3840, 1600), "21:9 nearest 1600");
    assert!(adjust(RowId::Resolution, 1, false, &mut ctx));
    assert_eq!(size(&ctx), (5120, 2160), "steps inside 21:9");
    assert!(
        !adjust(RowId::Resolution, 1, false, &mut ctx),
        "clamps at the family's end"
    );
    ctx.settings.match_window = true;
    (ctx.settings.width, ctx.settings.height) = (0, 0);
    assert!(adjust(RowId::Aspect, -1, true, &mut ctx));
    assert_eq!(size(&ctx), (1600, 1200), "wrapped to 4:3");
    assert!(!ctx.settings.match_window, "a size clears the policy");
}

/// A phone leads the Aspect row with its own screen and safe area, and Native
/// (safe area) reads as the latter.
#[test]
fn a_phone_leads_the_aspect_row_with_its_own_shapes() {
    let mut settings = Settings::default();
    let library = crate::library::LibraryShared::default();
    let device = crate::screens::Device {
        platform: crate::platform::Platform::Android,
        screen: Some(crate::shell::DeviceScreen {
            full: (3216, 1440),
            safe: (3088, 1440),
        }),
        ..crate::screens::Device::test()
    };
    let mut ctx = Ctx {
        device: &device,
        ..Ctx::test(&mut settings, &library)
    };
    let aspect = |ctx: &Ctx| {
        row_spec(RowId::Aspect, ctx, &[], &Default::default())
            .value
            .unwrap_or_default()
    };
    assert_eq!(aspect(&ctx), "Screen", "Native lists the screen");
    set_extra_bool(ctx.settings, device_keys::SAFE_AREA_MODE, true);
    assert_eq!(aspect(&ctx), "Safe area");
    assert!(adjust(RowId::Aspect, -1, false, &mut ctx));
    assert_eq!(
        (ctx.settings.width, ctx.settings.height),
        (2412, 1080),
        "Screen nearest 1080"
    );
    assert!(adjust(RowId::Resolution, 1, false, &mut ctx));
    assert_eq!((ctx.settings.width, ctx.settings.height), (3216, 1440));
    assert!(adjust(RowId::Aspect, 1, false, &mut ctx));
    assert_eq!(aspect(&ctx), "Safe area");
    assert!(adjust(RowId::Aspect, 1, false, &mut ctx));
    assert_eq!(aspect(&ctx), "16:9");
}

#[test]
fn toggles_read_left_off_right_on() {
    let mut settings = Settings {
        mic_enabled: false,
        ..Settings::default()
    };
    let library = crate::library::LibraryShared::default();
    let mut ctx = Ctx::test(&mut settings, &library);
    assert!(
        !adjust(RowId::Mic, -1, false, &mut ctx),
        "already off = thud"
    );
    assert!(adjust(RowId::Mic, 1, false, &mut ctx));
    assert!(ctx.settings.mic_enabled);
    assert!(adjust(RowId::Mic, 1, true, &mut ctx), "A always flips");
    assert!(!ctx.settings.mic_enabled);
}

#[test]
fn echo_cancellation_follows_the_microphone() {
    let mut settings = Settings {
        mic_enabled: false,
        ..Settings::default()
    };
    assert!(settings.echo_cancel, "it ships on");
    let library = crate::library::LibraryShared::default();
    let mut ctx = Ctx::test(&mut settings, &library);
    assert!(!row_spec(RowId::EchoCancel, &ctx, &[], &Default::default()).enabled);
    assert!(
        !adjust(RowId::EchoCancel, -1, false, &mut ctx),
        "mic off = thud"
    );
    assert!(!adjust(RowId::EchoCancel, 1, true, &mut ctx), "A too");
    assert!(ctx.settings.echo_cancel, "and nothing was written");

    ctx.settings.mic_enabled = true;
    assert!(row_spec(RowId::EchoCancel, &ctx, &[], &Default::default()).enabled);
    assert!(adjust(RowId::EchoCancel, -1, false, &mut ctx));
    assert!(!ctx.settings.echo_cancel);
    assert!(adjust(RowId::EchoCancel, 1, true, &mut ctx));
    assert!(ctx.settings.echo_cancel);
}

/// The TV's codec row wraps from H.264 back to Automatic: no AV1, no PyroWave.
#[test]
fn webos_offers_only_the_codecs_ndl_decodes() {
    let mut settings = Settings {
        codec: "h264".into(),
        ..Settings::default()
    };
    let library = crate::library::LibraryShared::default();
    let device = crate::screens::Device {
        platform: crate::platform::Platform::WebOS,
        fallback_ui: true,
        ..crate::screens::Device::test()
    };
    let mut ctx = Ctx {
        device: &device,
        ..Ctx::test(&mut settings, &library)
    };
    assert!(adjust(RowId::Codec, 1, true, &mut ctx));
    assert_eq!(ctx.settings.codec, "auto");
    let desktop = crate::screens::Device::test();
    let mut ctx = Ctx {
        device: &desktop,
        ..ctx
    };
    ctx.settings.codec = "h264".into();
    assert!(adjust(RowId::Codec, 1, true, &mut ctx));
    assert_eq!(ctx.settings.codec, "av1");
}

/// A GPU without the codec's compute set says so on the row, in both places a user
/// reads: the value and the line under the list. The other codecs are untouched.
#[test]
fn pyrowave_reads_unsupported_where_the_gpu_cannot_decode_it() {
    let mut settings = Settings {
        codec: "pyrowave".into(),
        ..Settings::default()
    };
    let library = crate::library::LibraryShared::default();
    let device = crate::screens::Device {
        platform: crate::platform::Platform::Android,
        pyrowave_ok: false,
        av1_ok: false,
        ..crate::screens::Device::test()
    };
    let ctx = Ctx {
        device: &device,
        ..Ctx::test(&mut settings, &library)
    };
    let value = row_spec(RowId::Codec, &ctx, &[], &Default::default())
        .value
        .unwrap();
    assert!(value.contains("unsupported"), "value said {value}");
    assert!(detail(RowId::Codec, &ctx).contains("can't decode PyroWave"));

    ctx.settings.codec = "hevc".into();
    assert_eq!(
        row_spec(RowId::Codec, &ctx, &[], &Default::default())
            .value
            .unwrap(),
        "HEVC"
    );
    assert!(!detail(RowId::Codec, &ctx).contains("PyroWave"));

    ctx.settings.codec = "pyrowave".into();
    let decodes = crate::screens::Device {
        pyrowave_ok: true,
        ..device.clone()
    };
    let ctx = Ctx {
        device: &decodes,
        ..ctx
    };
    assert_eq!(
        row_spec(RowId::Codec, &ctx, &[], &Default::default())
            .value
            .unwrap(),
        "PyroWave (wired LAN)"
    );
}

/// A device with no hardware AV1 decoder never advertises AV1, so the row that still
/// says "AV1" is the bug (#1138): the value and the line under it both say it lost.
#[test]
fn av1_reads_unsupported_without_a_hardware_decoder() {
    let mut settings = Settings {
        codec: "av1".into(),
        ..Settings::default()
    };
    let library = crate::library::LibraryShared::default();
    let device = crate::screens::Device {
        av1_ok: false,
        ..crate::screens::Device::test()
    };
    let ctx = Ctx {
        device: &device,
        ..Ctx::test(&mut settings, &library)
    };
    let value = row_spec(RowId::Codec, &ctx, &[], &Default::default())
        .value
        .unwrap();
    assert_eq!(value, "AV1 (unsupported)");
    assert!(detail(RowId::Codec, &ctx).contains("no hardware AV1 decoder"));

    // The other codecs keep the plain row and the plain line.
    ctx.settings.codec = "hevc".into();
    assert_eq!(
        row_spec(RowId::Codec, &ctx, &[], &Default::default())
            .value
            .unwrap(),
        "HEVC"
    );
    assert!(!detail(RowId::Codec, &ctx).contains("AV1"));

    ctx.settings.codec = "av1".into();
    let decodes = crate::screens::Device::test();
    let ctx = Ctx {
        device: &decodes,
        ..ctx
    };
    assert_eq!(
        row_spec(RowId::Codec, &ctx, &[], &Default::default())
            .value
            .unwrap(),
        "AV1"
    );
}

#[test]
fn bitrate_dims_under_pyrowave() {
    let mut settings = Settings {
        codec: "pyrowave".into(),
        bitrate_kbps: 80_000,
        ..Settings::default()
    };
    let library = crate::library::LibraryShared::default();
    let mut ctx = Ctx::test(&mut settings, &library);
    assert!(!row_spec(RowId::Bitrate, &ctx, &[], &Default::default()).enabled);
    assert!(
        !adjust(RowId::Bitrate, 1, false, &mut ctx),
        "pyrowave = thud"
    );
    assert!(!adjust(RowId::Bitrate, 1, true, &mut ctx), "A too");
    assert_eq!(ctx.settings.bitrate_kbps, 80_000, "the stored rate is kept");

    ctx.settings.codec = "hevc".into();
    assert!(row_spec(RowId::Bitrate, &ctx, &[], &Default::default()).enabled);
    assert!(adjust(RowId::Bitrate, 1, false, &mut ctx));
}

#[test]
fn smoothness_buffer_is_offered_only_under_smoothness() {
    let mut settings = Settings {
        show_advanced: true,
        ..Settings::default()
    };
    assert_eq!(settings.present_priority, "latency", "the shipped default");
    let library = crate::library::LibraryShared::default();
    let mut ctx = Ctx::test(&mut settings, &library);
    let mut s = SettingsScreen::with_presets(Vec::new());
    s.tab = TABS
        .iter()
        .position(|(name, _)| *name == "Display")
        .expect("the Picture section");

    let video = s.row_ids(&ctx);
    assert!(
        !video.contains(&RowId::SmoothBuffer),
        "latency hides the buffer row: {video:?}"
    );
    assert!(video.contains(&RowId::PresentPriority), "the intent stays");
    assert!(
        !adjust(RowId::SmoothBuffer, 1, false, &mut ctx),
        "latency intent = thud"
    );
    assert_eq!(ctx.settings.smooth_buffer, 0, "and nothing was written");

    assert!(adjust(RowId::PresentPriority, 1, false, &mut ctx));
    assert_eq!(ctx.settings.present_priority, "smooth");
    let video = s.row_ids(&ctx);
    let intent = video
        .iter()
        .position(|id| *id == RowId::PresentPriority)
        .expect("the intent row");
    assert_eq!(
        video.get(intent + 1),
        Some(&RowId::SmoothBuffer),
        "the row that comes and goes sits BELOW the row that decides it, so the cursor \
         never has anything move out from under it"
    );
    assert!(adjust(RowId::SmoothBuffer, 1, false, &mut ctx));
    assert_eq!(ctx.settings.smooth_buffer, 1);

    s.list.cursor = intent;
    assert!(adjust(RowId::PresentPriority, -1, false, &mut ctx));
    assert_eq!(ctx.settings.present_priority, "latency");
    let video = s.row_ids(&ctx);
    assert!(!video.contains(&RowId::SmoothBuffer));
    assert_eq!(
        video.get(s.list.cursor),
        Some(&RowId::PresentPriority),
        "the cursor is still on the row the user was stepping"
    );
}

/// Cursor past a list that shrank: pull back, do not index. Another writer can
/// flip presentation intent while this screen is open.
#[test]
fn a_shrinking_list_pulls_the_cursor_back() {
    // Seat the STORE with the shrunken list: `apply_row` rebases on it.
    fake_home();
    let mut settings = Settings {
        present_priority: "latency".into(),
        show_advanced: true,
        ..Settings::default()
    };
    crate::store::file_store().save(&settings);
    settings.present_priority = "smooth".into();
    let library = crate::library::LibraryShared::default();
    let mut ctx = Ctx::test(&mut settings, &library);
    let mut s = SettingsScreen::with_presets(Vec::new());
    s.tab = TABS
        .iter()
        .position(|(name, _)| *name == "Display")
        .expect("the Picture section");
    s.list.cursor = s.row_ids(&ctx).len() - 1;
    let parked = s.list.cursor;
    ctx.settings.present_priority = "latency".into();
    let mut fx = Outbox::default();
    let pulse = s.menu(MenuEvent::Confirm, &mut ctx, &mut fx);
    assert!(pulse.is_some(), "the press was routed, not dropped");
    assert!(s.list.cursor < parked, "the cursor came back onto the list");
    assert!(fx.nav.is_none());
}

#[test]
fn touch_mode_steps_and_wraps() {
    let mut settings = Settings::default();
    assert_eq!(settings.touch_mode, "trackpad");
    let library = crate::library::LibraryShared::default();
    let mut ctx = Ctx::test(&mut settings, &library);
    assert!(
        !adjust(RowId::Touch, -1, false, &mut ctx),
        "already first = thud"
    );
    assert!(adjust(RowId::Touch, 1, false, &mut ctx));
    assert_eq!(ctx.settings.touch_mode, "pointer");
    assert!(adjust(RowId::Touch, 1, false, &mut ctx));
    assert_eq!(ctx.settings.touch_mode, "touch");
    assert!(adjust(RowId::Touch, 1, false, &mut ctx));
    assert_eq!(ctx.settings.touch_mode, "off");
    assert!(!adjust(RowId::Touch, 1, false, &mut ctx), "last = thud");
    assert!(adjust(RowId::Touch, 1, true, &mut ctx));
    assert_eq!(ctx.settings.touch_mode, "trackpad");
}

#[test]
fn mouse_mode_steps_and_wraps() {
    let mut settings = Settings::default();
    assert_eq!(settings.mouse_mode, "capture");
    let library = crate::library::LibraryShared::default();
    let mut ctx = Ctx::test(&mut settings, &library);
    assert!(
        !adjust(RowId::Mouse, -1, false, &mut ctx),
        "already first = thud"
    );
    assert!(adjust(RowId::Mouse, 1, false, &mut ctx));
    assert_eq!(ctx.settings.mouse_mode, "desktop");
    assert!(!adjust(RowId::Mouse, 1, false, &mut ctx), "last = thud");
    assert!(adjust(RowId::Mouse, 1, true, &mut ctx));
    assert_eq!(ctx.settings.mouse_mode, "capture");
}

/// Off-ladder must not snap to Automatic (index 0). Step to the neighbour.
#[test]
fn an_off_ladder_rate_steps_to_its_neighbour() {
    let mut settings = Settings {
        bitrate_kbps: 12_345,
        ..Settings::default()
    };
    let library = crate::library::LibraryShared::default();
    let mut ctx = Ctx::test(&mut settings, &library);
    assert!(adjust(RowId::Bitrate, 1, false, &mut ctx));
    assert_eq!(ctx.settings.bitrate_kbps, 15_000, "the rung above");
    ctx.settings.bitrate_kbps = 12_345;
    assert!(adjust(RowId::Bitrate, -1, false, &mut ctx));
    assert_eq!(ctx.settings.bitrate_kbps, 12_000, "the rung below");
    ctx.settings.bitrate_kbps = 2_000_000;
    assert!(!adjust(RowId::Bitrate, 1, false, &mut ctx), "the ceiling");
    ctx.settings.bitrate_kbps = 5_000;
    assert!(adjust(RowId::Bitrate, -1, false, &mut ctx));
    assert_eq!(ctx.settings.bitrate_kbps, 4_000);
}

#[test]
fn a_typed_bitrate_is_stored_and_clamped() {
    let mut settings = Settings::default();
    let library = crate::library::LibraryShared::default();
    // Snapshot, not the file store: this test saves.
    let store = crate::store::SnapshotStore::new(settings.clone(), Vec::new());
    let mut ctx = Ctx {
        store: &store,
        ..Ctx::test(&mut settings, &library)
    };
    let mut s = SettingsScreen::with_presets(Vec::new());
    s.tab = tab_of(RowId::Bitrate);
    let mut fx = Outbox::default();
    let ids = s.row_ids(&ctx);
    s.list.cursor = ids
        .iter()
        .position(|id| *id == RowId::Bitrate)
        .expect("the bitrate row");
    s.menu(MenuEvent::Secondary, &mut ctx, &mut fx);
    assert!(s.editing(), "Y opens the field");
    s.text_input("13x7"); // digits only: 'x' is refused
    assert!(s.edit_key(crate::input::Key::Return, &mut ctx));
    assert!(!s.editing(), "Return closes it");
    assert_eq!(ctx.settings.bitrate_kbps, 137_000);

    s.menu(MenuEvent::Secondary, &mut ctx, &mut fx);
    s.text_input("99999"); // four digits; the fifth is refused
    assert!(s.edit_key(crate::input::Key::Return, &mut ctx));
    assert_eq!(
        ctx.settings.bitrate_kbps, 2_000_000,
        "clamped to the ceiling"
    );

    s.menu(MenuEvent::Secondary, &mut ctx, &mut fx);
    assert!(s.edit_key(crate::input::Key::Return, &mut ctx));
    assert_eq!(ctx.settings.bitrate_kbps, 2_000_000, "left alone");

    s.list.cursor = 0;
    s.menu(MenuEvent::Secondary, &mut ctx, &mut fx);
    assert!(!s.editing());
}

/// Y on Resolution takes a width, then a height, through the shared rule. The size then reads
/// Custom, and a step leaves it for its list neighbour, never for Native.
#[test]
fn a_typed_size_is_stored_through_the_shared_rule() {
    let mut settings = Settings {
        codec: "h264".into(),
        match_window: true,
        ..Settings::default()
    };
    let library = crate::library::LibraryShared::default();
    let store = crate::store::SnapshotStore::new(settings.clone(), Vec::new());
    let mut ctx = Ctx {
        store: &store,
        ..Ctx::test(&mut settings, &library)
    };
    let mut s = SettingsScreen::with_presets(Vec::new());
    s.tab = tab_of(RowId::Resolution);
    let mut fx = Outbox::default();
    let ids = s.row_ids(&ctx);
    s.list.cursor = ids
        .iter()
        .position(|id| *id == RowId::Resolution)
        .expect("the resolution row");

    s.menu(MenuEvent::Secondary, &mut ctx, &mut fx);
    assert!(s.edit_key(crate::input::Key::Return, &mut ctx));
    assert!(!s.editing(), "an empty width abandons the edit");
    assert!(ctx.settings.match_window, "and changes nothing");

    s.menu(MenuEvent::Secondary, &mut ctx, &mut fx);
    assert_eq!(s.edit_field().expect("open").label, "Width in pixels");
    s.text_input("5121");
    assert!(s.edit_key(crate::input::Key::Return, &mut ctx));
    assert_eq!(
        s.edit_field().expect("still open").label,
        "Height in pixels",
        "the width moves on to the height"
    );
    s.text_input("1601");
    assert!(s.edit_key(crate::input::Key::Return, &mut ctx));
    assert!(!s.editing());
    assert_eq!(
        (ctx.settings.width, ctx.settings.height),
        (4096, 1600),
        "even, and within H.264's 4096 a side"
    );
    assert!(!ctx.settings.match_window);
    let spec = row_spec(RowId::Resolution, &ctx, &[], &Default::default());
    assert_eq!(spec.value.as_deref(), Some("Custom (4096 × 1600)"));

    assert!(
        !adjust(RowId::Resolution, 1, false, &mut ctx),
        "the typed size is the last entry"
    );
    assert!(adjust(RowId::Resolution, -1, false, &mut ctx));
    assert_eq!(
        (ctx.settings.width, ctx.settings.height),
        (5120, 2880),
        "the largest listed size, not Native"
    );
}

/// Advanced rows stay out of a tab until Show advanced. A changed one surfaces as a count at
/// the tab's end, which shows them and lands on it; the first advanced row carries the heading.
#[test]
fn advanced_rows_hide_until_shown_and_a_changed_one_is_counted() {
    let mut settings = Settings::default();
    let library = crate::library::LibraryShared::default();
    let store = crate::store::SnapshotStore::new(settings.clone(), Vec::new());
    let mut ctx = Ctx {
        store: &store,
        ..Ctx::test(&mut settings, &library)
    };
    let mut s = SettingsScreen::with_presets(Vec::new());
    s.tab = tab_of(RowId::Codec);
    let ids = s.row_ids(&ctx);
    assert!(ids.iter().all(|id| !advanced(*id)), "{ids:?}");
    assert!(
        !ids.contains(&RowId::AdvancedChanged),
        "nothing changed yet"
    );

    ctx.settings.codec = "av1".into();
    let ids = s.row_ids(&ctx);
    assert_eq!(ids.last(), Some(&RowId::AdvancedChanged));
    assert_eq!(
        s.spec(RowId::AdvancedChanged, &ids, &ctx).label,
        "1 advanced setting changed"
    );

    s.list.jump_to(ids.len() - 1);
    let mut fx = Outbox::default();
    s.menu(MenuEvent::Confirm, &mut ctx, &mut fx);
    assert!(ctx.settings.show_advanced);
    let ids = s.row_ids(&ctx);
    assert_eq!(ids.get(s.list.cursor), Some(&RowId::Codec), "lands on it");
    let first = ids.iter().copied().find(|id| advanced(*id)).unwrap();
    assert_eq!(s.spec(first, &ids, &ctx).header, Some("Advanced"));
}

/// A platform's own fresh value is not a change: Android starts the pad speaker off.
#[test]
fn a_platform_default_is_not_counted_as_changed() {
    let mut settings = Settings {
        pad_speaker: "off".into(),
        ..Settings::default()
    };
    let library = crate::library::LibraryShared::default();
    let android = crate::screens::Device {
        platform: crate::platform::Platform::Android,
        ..crate::screens::Device::test()
    };
    let ctx = Ctx {
        device: &android,
        ..Ctx::test(&mut settings, &library)
    };
    assert!(changed_advanced(tab_of(RowId::PadSpeaker), &ctx).is_empty());
}

#[test]
fn rates_read_in_the_biggest_round_unit() {
    assert_eq!(bitrate_label(20_000), "20 Mbps");
    assert_eq!(bitrate_label(12_500), "12.5 Mbps");
    assert_eq!(bitrate_label(1_000_000), "1 Gbps");
    assert_eq!(bitrate_label(1_500_000), "1.5 Gbps");
    assert_eq!(bitrate_label(2_000_000), "2 Gbps");
}

#[test]
fn preset_rows_navigate_instead_of_editing() {
    let mut settings = Settings::default();
    let library = crate::library::LibraryShared::default();
    let mut pinned = crate::model::HostRow {
        key: crate::model::pinned_key("aa", "p1"),
        pin: Some(crate::model::PresetChip {
            id: "p1".into(),
            name: "Work".into(),
            accent: None,
            bitrate_kbps: None,
        }),
        ..crate::model::HostRow::fixture("aa", "Tower")
    };
    let hosts = [pinned.clone(), {
        pinned.key = "aa".into();
        pinned.pin = None;
        pinned
    }];
    let mut ctx = Ctx {
        hosts: &hosts,
        ..Ctx::test(&mut settings, &library)
    };
    let mut s = SettingsScreen::with_presets(vec![
        ("p1".into(), "Work".into()),
        ("p2".into(), "Game".into()),
    ]);
    s.tab = PRESETS_TAB;
    let ids = s.row_ids(&ctx);
    assert_eq!(
        ids,
        vec![RowId::Preset(0), RowId::Preset(1), RowId::NewPreset]
    );

    let spec = row_spec(RowId::Preset(0), &ctx, &s.presets, &s.overrides);
    assert_eq!(spec.header, None, "the tab pill names the section");
    assert_eq!(spec.label, "Work");
    assert_eq!(spec.value.as_deref(), Some("Pinned to 1 host"));
    let spec = row_spec(RowId::Preset(1), &ctx, &s.presets, &s.overrides);
    assert_eq!(spec.value.as_deref(), Some("Not pinned"));

    s.list.cursor = 0;
    let mut fx = Outbox::default();
    s.menu(MenuEvent::Confirm, &mut ctx, &mut fx);
    assert!(
        matches!(fx.nav, Some(crate::screens::Nav::Push(b))
            if matches!(*b, Screen::PresetMenu(_))),
        "A on a preset row opens its menu"
    );

    let mut fx = Outbox::default();
    let pulse = s.menu(
        MenuEvent::Move(pf_client_core::menu_nav::MenuDir::Right),
        &mut ctx,
        &mut fx,
    );
    assert!(matches!(pulse, Some(MenuPulse::Boundary)));
    assert!(fx.nav.is_none() && fx.cmds.is_empty());
}

/// With no presets the tab still offers New preset, which opens the name screen.
#[test]
fn empty_catalog_offers_a_new_preset() {
    let mut settings = Settings::default();
    let library = crate::library::LibraryShared::default();
    let mut ctx = Ctx::test(&mut settings, &library);
    let mut s = SettingsScreen::with_presets(Vec::new());
    s.tab = PRESETS_TAB;
    let ids = s.row_ids(&ctx);
    assert_eq!(ids, vec![RowId::NewPreset]);
    let spec = row_spec(RowId::NewPreset, &ctx, &s.presets, &s.overrides);
    assert!(spec.enabled);

    s.list.cursor = ids.len() - 1;
    let mut fx = Outbox::default();
    s.menu(MenuEvent::Confirm, &mut ctx, &mut fx);
    assert!(matches!(fx.nav, Some(crate::screens::Nav::Push(b))
        if matches!(*b, Screen::PresetName(_))));
}

#[test]
fn the_quick_actions_row_opens_the_editor_and_steps_nothing() {
    let mut settings = Settings::default();
    let library = crate::library::LibraryShared::default();
    let mut ctx = Ctx::test(&mut settings, &library);
    let row = row_spec(RowId::QuickActions, &ctx, &[], &Default::default());
    assert!(row.value.is_none(), "an action row");
    assert_eq!(row.label, "Quick actions");
    assert!(!adjust(RowId::QuickActions, 1, false, &mut ctx));
    assert!(
        ctx.settings.overlay_actions.is_empty(),
        "the row itself never writes the blob"
    );
    assert!(TABS
        .iter()
        .any(|(tab, rows)| *tab == "Input" && rows.contains(&RowId::QuickActions)));
}

#[test]
fn platform_row_split_hides_only_the_other_platforms_concepts() {
    use crate::platform::Platform;
    let all: Vec<RowId> = TABS
        .iter()
        .flat_map(|(_, rows)| rows.iter().copied())
        .collect();
    let off_desktop: Vec<RowId> = all
        .iter()
        .copied()
        .filter(|id| !row_on(*id, Platform::Desktop))
        .collect();
    assert_eq!(
        off_desktop,
        vec![
            RowId::FullscreenMode,
            RowId::BackgroundKeepAlive,
            RowId::BackgroundTimeout,
            RowId::GamepadUi,
            RowId::GamepadUiMode,
            RowId::ReduceUiResolution,
            RowId::SecondScreen,
            RowId::LowLatency,
            RowId::AudioRoute,
            RowId::CursorGestures,
            RowId::Controllers,
            RowId::PhoneRumble,
            RowId::PhoneGyro,
            RowId::Sc2Passthrough,
            RowId::DsCapture,
        ]
    );
    let off_android: Vec<RowId> = all
        .iter()
        .copied()
        .filter(|id| !row_on(*id, Platform::Android))
        .collect();
    assert_eq!(
        off_android,
        vec![
            RowId::FullscreenMode,
            RowId::Fullscreen,
            RowId::Chroma444,
            // TenBitSdr is NOT here: MediaCodec decodes Main10 from the SPS and the depth
            // asks nothing of the panel, so Android obeys it.
            RowId::Vsync,
            RowId::AllowVrr,
            RowId::Decoder,
            RowId::AudioRoute,
            RowId::Shortcuts,
            RowId::CursorGestures,
            // Every controller already gets its own wire slot, so player 1 is not a choice.
            RowId::Pad,
        ]
    );
    // Every row reaches at least one platform: a row listed in a tab and offered nowhere
    // is dead weight the tab still spends a line on.
    assert!(all
        .iter()
        .all(|id| Platform::ALL.iter().any(|p| row_on(*id, *p))));
}

/// The other direction, and the one that bites: a platform missing from every list
/// offers no row at all, so all six tabs draw empty instead of one control going
/// missing. `Web` shipped that way.
#[test]
fn every_platform_offers_rows() {
    use crate::platform::Platform;
    for p in Platform::ALL {
        // Exhaustive on purpose: a new variant must be weighed here and added to `ALL`.
        match p {
            Platform::Desktop
            | Platform::Android
            | Platform::WebOS
            | Platform::Web
            | Platform::Apple
            | Platform::Tizen => {}
        }
        let n = TABS
            .iter()
            .flat_map(|(_, rows)| rows.iter())
            .filter(|id| row_on(**id, p))
            .count();
        assert!(n > 0, "{p:?} offers no settings rows at all");
    }
}

/// A Samsung set fronts the same page as the browser, so it answers the row tables exactly as
/// Web does; it is held by a remote and must exit on Back at the root, so those two answers are
/// the TV's. The codec row offers what the set decodes: no AV1, no PyroWave.
#[test]
fn tizen_is_web_with_a_remote_and_an_exit() {
    use crate::glyphs::GlyphStyle;
    use crate::platform::Platform;
    for id in TABS.iter().flat_map(|(_, rows)| rows.iter()) {
        assert_eq!(
            row_on(*id, Platform::Tizen),
            row_on(*id, Platform::Web),
            "{id:?}: a packaged page offers what the page offers"
        );
    }
    assert!(
        Platform::Tizen.can_quit(),
        "Back at the root exits a Samsung app"
    );
    assert!(!Platform::Web.can_quit());
    assert_eq!(GlyphStyle::keys(Platform::Tizen), GlyphStyle::Remote);
    assert_eq!(GlyphStyle::keys(Platform::Web), GlyphStyle::Keyboard);
    let offered: Vec<&str> = codecs(Platform::Tizen).iter().map(|(id, _)| *id).collect();
    assert_eq!(offered, ["auto", "hevc", "h264"]);
    assert_eq!(
        own_stats_corner(Platform::Tizen),
        own_stats_corner(Platform::Web)
    );
    assert_eq!(
        bitrate_ceiling_kbps(Platform::Tizen),
        bitrate_ceiling_kbps(Platform::Web)
    );
}

#[test]
fn android_rows_live_in_extra() {
    let mut settings = Settings::default();
    let library = crate::library::LibraryShared::default();
    let android = crate::screens::Device {
        platform: crate::platform::Platform::Android,
        ..crate::screens::Device::test()
    };
    let mut c = Ctx {
        device: &android,
        ..Ctx::test(&mut settings, &library)
    };
    {
        let ctx = &mut c;
        let before = ctx.settings.clone();
        assert!(extra_bool(ctx.settings, device_keys::LOW_LATENCY, true));
        assert!(adjust(RowId::LowLatency, 1, true, ctx));
        assert!(!extra_bool(ctx.settings, device_keys::LOW_LATENCY, true));
        assert!(adjust(RowId::GamepadUiMode, 1, true, ctx));
        assert_eq!(
            extra_str(ctx.settings, GAMEPAD_UI_MODE_KEY, "connected"),
            "always"
        );
        assert!(extra_bool(ctx.settings, GAMEPAD_UI_KEY, true));
        assert!(adjust(RowId::GamepadUi, 1, true, ctx));
        assert!(!extra_bool(ctx.settings, GAMEPAD_UI_KEY, true));
        let mut after = ctx.settings.clone();
        after.extra = before.extra.clone();
        assert_eq!(after, before);
    }
}

/// A Mac's tab offers the picker instead of the toggle; its preset editor keeps the toggle,
/// since "Always" is this Mac's alone.
#[test]
fn mac_fullscreen_picker_steps_through_both_keys() {
    let mut settings = Settings::default();
    let library = crate::library::LibraryShared::default();
    let mac = crate::screens::Device {
        platform: crate::platform::Platform::Apple,
        ..crate::screens::Device::test()
    };
    let tv = crate::screens::Device {
        tv: true,
        ..mac.clone()
    };
    let ctx = &mut Ctx {
        device: &mac,
        ..Ctx::test(&mut settings, &library)
    };
    let interface = TABS.iter().position(|(t, _)| *t == "General").unwrap();
    let mut s = SettingsScreen::with_presets(Vec::new());
    s.tab = interface;
    let tab = s.row_ids(ctx);
    assert!(tab.contains(&RowId::FullscreenMode) && !tab.contains(&RowId::Fullscreen));
    let presets: Vec<RowId> = preset_rows(ctx).into_iter().map(|(_, id)| id).collect();
    assert!(presets.contains(&RowId::Fullscreen));
    assert!(!presets.contains(&RowId::FullscreenMode));

    assert!(ctx.settings.fullscreen_on_stream);
    assert_eq!(fullscreen_mode(ctx.settings), 1);
    assert!(adjust(RowId::FullscreenMode, 1, false, ctx));
    assert!(extra_bool(ctx.settings, FULLSCREEN_ALWAYS_KEY, false));
    assert!(
        !adjust(RowId::FullscreenMode, 1, false, ctx),
        "Always is the end"
    );
    assert!(adjust(RowId::FullscreenMode, -2, false, ctx));
    assert!(!extra_bool(ctx.settings, FULLSCREEN_ALWAYS_KEY, true));
    assert!(!ctx.settings.fullscreen_on_stream);

    ctx.device = &tv;
    assert!(!row_applies(RowId::FullscreenMode, ctx));
}

#[test]
fn console_off_switch_needs_a_fallback_ui() {
    let mut settings = Settings::default();
    let library = crate::library::LibraryShared::default();
    let tv = crate::screens::Device {
        platform: crate::platform::Platform::Android,
        ..crate::screens::Device::test()
    };
    let ctx = Ctx {
        device: &tv,
        ..Ctx::test(&mut settings, &library)
    };
    assert!(
        !row_applies(RowId::GamepadUi, &ctx),
        "a TV offers no off switch"
    );
    assert!(!row_applies(RowId::GamepadUiMode, &ctx));
    let phone = crate::screens::Device {
        fallback_ui: true,
        ..tv.clone()
    };
    let ctx = Ctx {
        device: &phone,
        ..ctx
    };
    assert!(row_applies(RowId::GamepadUi, &ctx));
    assert!(row_applies(RowId::GamepadUiMode, &ctx));
    set_extra_bool(ctx.settings, GAMEPAD_UI_KEY, false);
    assert!(row_applies(RowId::GamepadUi, &ctx));
    assert!(
        !row_applies(RowId::GamepadUiMode, &ctx),
        "the mode row decides nothing while the switch above it is off"
    );
}

/// webOS bounds its own slider at 200 Mbps and clamps the document to it, so the shell
/// must not offer more there — and must not take anything away from the others.
#[test]
fn the_bitrate_ceiling_is_webos_only() {
    use crate::platform::Platform;
    assert_eq!(bitrate_ceiling_kbps(Platform::WebOS), 200_000);
    assert_eq!(
        bitrate_ceiling_kbps(Platform::Desktop),
        *BITRATES.last().expect("rungs"),
        "the ladder's own top still stands off the TV"
    );
    assert_eq!(
        bitrate_ceiling_kbps(Platform::Android),
        bitrate_ceiling_kbps(Platform::Desktop)
    );

    // Stepping up from the rung below the cap lands ON it and goes no further.
    let mut settings = Settings::default();
    let library = crate::library::LibraryShared::default();
    let webos = crate::screens::Device {
        platform: Platform::WebOS,
        ..crate::screens::Device::test()
    };
    {
        let ctx = &mut Ctx {
            device: &webos,
            ..Ctx::test(&mut settings, &library)
        };
        ctx.settings.bitrate_kbps = 150_000;
        assert!(adjust(RowId::Bitrate, 1, false, ctx));
        assert_eq!(ctx.settings.bitrate_kbps, 200_000);
        assert!(
            !adjust(RowId::Bitrate, 1, false, ctx),
            "200 Mbps is the last rung a TV may reach"
        );
        assert_eq!(ctx.settings.bitrate_kbps, 200_000);
        // Down still works, so the cap is a ceiling and not a trap.
        assert!(adjust(RowId::Bitrate, -1, false, ctx));
        assert_eq!(ctx.settings.bitrate_kbps, 150_000);
    }

    // The same step on a desktop keeps climbing.
    with_ctx(|ctx| {
        ctx.settings.bitrate_kbps = 200_000;
        assert!(adjust(RowId::Bitrate, 1, false, ctx));
        assert_eq!(ctx.settings.bitrate_kbps, 250_000);
    });
}

#[test]
fn every_row_has_exactly_one_tab() {
    let mut seen: Vec<RowId> = Vec::new();
    for (_, rows) in &TABS {
        for id in *rows {
            assert!(!seen.contains(id), "{id:?} is in two tabs");
            seen.push(*id);
        }
    }
    assert_eq!(seen.len(), 68, "{seen:?}");
    assert!(
        !seen.contains(&RowId::AdvancedChanged),
        "built per tab, never listed"
    );
    assert!(seen.contains(&RowId::StartIn));
    assert!(seen.contains(&RowId::AdvancedStats));
    assert!(seen.contains(&RowId::FollowOsTheme));
    assert!(seen.contains(&RowId::Palette));
    assert!(seen.contains(&RowId::ReduceMotion));
    assert!(seen.contains(&RowId::ReduceUiResolution));
    assert!(seen.contains(&RowId::AudioFormat));
    assert!(TABS[PRESETS_TAB].1.is_empty());
    assert_eq!(TABS[PRESETS_TAB].0, "Presets");
}

/// Only test that touches the process-wide `os_theme` slot. A sibling races
/// under libtest. Leaves the slot cleared.
#[test]
fn the_follow_system_row_exists_only_where_a_theme_is_published() {
    with_ctx(|ctx| {
        assert!(
            !row_applies(RowId::FollowOsTheme, ctx),
            "no publisher, no row"
        );
        assert!(row_applies(RowId::Palette, ctx));

        let t = crate::os_theme::OsTheme {
            light: false,
            background: crate::os_theme::Rgb(0.02, 0.04, 0.12),
            foreground: crate::os_theme::Rgb(1.0, 0.81, 0.68),
            accent: crate::os_theme::Rgb(0.49, 0.51, 0.85),
        };
        crate::os_theme::set_os_theme(Some(t));
        let rev = crate::os_theme::os_theme().0;
        crate::os_theme::set_os_theme(Some(t));
        assert_eq!(
            crate::os_theme::os_theme().0,
            rev,
            "an unchanged publish is free"
        );

        assert!(row_applies(RowId::FollowOsTheme, ctx));
        assert!(
            !row_applies(RowId::Palette, ctx),
            "ruled by the system theme"
        );

        ctx.settings.follow_os_theme = false;
        assert!(row_applies(RowId::Palette, ctx));

        crate::os_theme::set_os_theme(None);
        assert!(!row_applies(RowId::FollowOsTheme, ctx));
    });
}

#[test]
fn shoulders_cycle_tabs_and_keep_each_cursor() {
    let mut settings = Settings::default();
    let library = crate::library::LibraryShared::default();
    let mut ctx = Ctx::test(&mut settings, &library);
    let mut s = SettingsScreen::with_presets(Vec::new());
    let mut fx = Outbox::default();
    assert_eq!(s.tab, 0);
    s.list.cursor = 3; // Stream / Bitrate
    s.menu(MenuEvent::JumpForward, &mut ctx, &mut fx);
    assert_eq!(s.tab, 1);
    assert_eq!(s.list.cursor, 0, "a fresh tab starts at its first row");
    s.list.cursor = 2; // Picture's third row
    s.menu(MenuEvent::JumpBack, &mut ctx, &mut fx);
    assert_eq!((s.tab, s.list.cursor), (0, 3), "Stream kept its place");
    s.menu(MenuEvent::JumpBack, &mut ctx, &mut fx);
    assert_eq!(s.tab, TABS.len() - 1, "About, the last");
    assert_eq!(s.list.cursor, 0);
    s.menu(MenuEvent::JumpForward, &mut ctx, &mut fx);
    assert_eq!(s.tab, 0);
    assert!(fx.nav.is_none() && fx.cmds.is_empty());
}

/// TV remotes have no shoulders and no Tab key: Up from row 0 focuses the strip.
#[test]
fn dpad_alone_reaches_every_tab() {
    let mut settings = Settings::default();
    let library = crate::library::LibraryShared::default();
    let mut ctx = Ctx::test(&mut settings, &library);
    let mut s = SettingsScreen::with_presets(Vec::new());
    let mut fx = Outbox::default();
    assert_eq!(s.list.cursor, 0);
    s.menu(MenuEvent::Move(MenuDir::Up), &mut ctx, &mut fx);
    assert!(s.strip_focus, "Up from the top row lands on the strip");
    s.menu(MenuEvent::Move(MenuDir::Right), &mut ctx, &mut fx);
    assert_eq!(s.tab, 1);
    assert!(s.strip_focus, "switching keeps the strip focused");
    s.menu(MenuEvent::Move(MenuDir::Left), &mut ctx, &mut fx);
    s.menu(MenuEvent::Move(MenuDir::Left), &mut ctx, &mut fx);
    assert_eq!(
        s.tab,
        TABS.len() - 1,
        "the strip wraps like the shoulders do"
    );
    s.menu(MenuEvent::Move(MenuDir::Down), &mut ctx, &mut fx);
    assert!(!s.strip_focus, "Down drops back into the list");
    s.menu(MenuEvent::Move(MenuDir::Down), &mut ctx, &mut fx);
    assert!(!s.strip_focus);
    assert!(fx.nav.is_none() && fx.cmds.is_empty());
}

/// Assert against `audio_format` constants so a spelling change there reds this
/// instead of writing a key nobody reads. Dim under surround: see [`row_spec`].
#[test]
fn audio_format_ships_off_and_follows_the_channel_count() {
    use pf_client_core::audio_format::{AUDIO_FORMAT_LOSSLESS_48, AUDIO_FORMAT_LOSSLESS_96};
    let mut settings = Settings::default();
    assert_eq!(settings.audio_format, AUDIO_FORMAT_OPUS, "off by default");
    assert_eq!(settings.audio_channels, 2, "…and the gate starts open");
    let library = crate::library::LibraryShared::default();
    let mut ctx = Ctx::test(&mut settings, &library);
    let mut s = SettingsScreen::with_presets(Vec::new());
    s.tab = tab_of(RowId::AudioFormat);
    assert!(
        !s.row_ids(&ctx).contains(&RowId::AudioFormat),
        "an advanced row: hidden by default"
    );
    ctx.settings.show_advanced = true;
    let audio = s.row_ids(&ctx);
    let first_advanced = audio.iter().position(|id| advanced(*id));
    assert_eq!(
        first_advanced.map(|i| audio[i]),
        Some(RowId::AudioFormat),
        "it leads the tab's advanced rows, the nearest it can sit to the row that dims it"
    );

    assert!(
        !adjust(RowId::AudioFormat, -1, false, &mut ctx),
        "already Opus = thud"
    );
    assert!(adjust(RowId::AudioFormat, 1, false, &mut ctx));
    assert_eq!(ctx.settings.audio_format, AUDIO_FORMAT_LOSSLESS_48);
    assert!(adjust(RowId::AudioFormat, 1, false, &mut ctx));
    assert_eq!(ctx.settings.audio_format, AUDIO_FORMAT_LOSSLESS_96);
    assert!(
        !adjust(RowId::AudioFormat, 1, false, &mut ctx),
        "last = thud"
    );
    assert!(adjust(RowId::AudioFormat, 1, true, &mut ctx));
    assert_eq!(ctx.settings.audio_format, AUDIO_FORMAT_OPUS);

    ctx.settings.audio_format = AUDIO_FORMAT_LOSSLESS_48.into();
    ctx.settings.audio_channels = 6;
    assert!(!row_spec(RowId::AudioFormat, &ctx, &[], &Default::default()).enabled);
    assert!(
        !adjust(RowId::AudioFormat, 1, false, &mut ctx),
        "surround = thud"
    );
    assert!(!adjust(RowId::AudioFormat, 1, true, &mut ctx), "A too");
    assert_eq!(
        ctx.settings.audio_format, AUDIO_FORMAT_LOSSLESS_48,
        "and nothing was written — the stored preference survives the gate"
    );
    assert!(s.row_ids(&ctx).contains(&RowId::AudioFormat));
    ctx.settings.audio_channels = 2;
    assert!(row_spec(RowId::AudioFormat, &ctx, &[], &Default::default()).enabled);

    ctx.settings.audio_format = AUDIO_FORMAT_OPUS.into();
    let opus = row_spec(RowId::AudioFormat, &ctx, &[], &Default::default()).value;
    assert!(opus.is_some());
    ctx.settings.audio_format = "lossless192".into();
    assert_eq!(
        row_spec(RowId::AudioFormat, &ctx, &[], &Default::default()).value,
        opus
    );
}

#[test]
fn palette_row_names_the_pick_and_opens_the_cards() {
    let mut settings = Settings::default();
    let library = crate::library::LibraryShared::default();
    let mut ctx = Ctx::test(&mut settings, &library);
    assert_eq!(ctx.settings.ui_palette, "violet", "the brand default ships");
    assert_eq!(
        row_spec(RowId::Palette, &ctx, &[], &Default::default())
            .value
            .as_deref(),
        Some("Violet")
    );
    assert!(
        !adjust(RowId::Palette, 1, true, &mut ctx),
        "a button, not a stepper"
    );
    assert_eq!(ctx.settings.ui_palette, "violet");
    ctx.settings.ui_palette = "chartreuse".into();
    assert_eq!(
        row_spec(RowId::Palette, &ctx, &[], &Default::default())
            .value
            .as_deref(),
        Some("Violet"),
        "an unknown palette reads as the default it actually draws"
    );
    // Confirm on the row pushes the picker, opened on the palette in force.
    let mut s = SettingsScreen::with_presets(Vec::new());
    s.tab = TABS
        .iter()
        .position(|(name, _)| *name == "General")
        .expect("the Interface section");
    let ids = s.row_ids(&ctx);
    s.list.cursor = ids
        .iter()
        .position(|r| *r == RowId::Palette)
        .expect("the Interface section lists Background");
    let mut fx = Outbox::default();
    s.apply_row(ListMsg::Activate, None, &ids, &mut ctx, &mut fx);
    assert!(matches!(fx.nav, Some(crate::screens::Nav::Push(ref b))
        if matches!(**b, Screen::Palette(_))));
}

/// About lists the stream controls on every platform, and a press opens the read-only screen.
#[test]
fn about_opens_the_stream_controls_where_the_client_has_them() {
    let mut settings = Settings::default();
    let library = crate::library::LibraryShared::default();
    let mut ctx = Ctx::test(&mut settings, &library);
    let mut s = SettingsScreen::with_presets(Vec::new());
    s.tab = TABS.len() - 1;
    let ids = s.row_ids(&ctx);
    s.list.cursor = ids
        .iter()
        .position(|r| *r == RowId::StreamControls)
        .expect("About lists the stream controls");
    let mut fx = Outbox::default();
    s.apply_row(ListMsg::Activate, None, &ids, &mut ctx, &mut fx);
    let Some(crate::screens::Nav::Push(b)) = fx.nav else {
        panic!("a press opens the screen");
    };
    assert!(matches!(*b, Screen::Licenses(ref l) if l.title() == "Stream controls"));

    for platform in crate::platform::Platform::ALL {
        let mut settings = Settings::default();
        let mut ctx = Ctx::test(&mut settings, &library);
        let mut device = ctx.device.clone();
        device.platform = platform;
        ctx.device = &device;
        assert!(
            s.row_ids(&ctx).contains(&RowId::StreamControls),
            "{platform:?}"
        );
    }
}

/// The value names where a launch will land, not what the key holds: with no
/// default host every setting resolves to the list, and the row must say so.
#[test]
fn the_start_in_row_cycles_and_names_the_host() {
    use pf_client_core::trust::{KnownHost, KnownHosts};

    let store = std::sync::Arc::new(crate::store::SnapshotStore::new(
        Settings::default(),
        Vec::new(),
    ));
    let mut settings = Settings::default();
    let library = crate::library::LibraryShared::default();
    let mut ctx = Ctx {
        store: store.as_ref(),
        ..Ctx::test(&mut settings, &library)
    };
    let value = |ctx: &Ctx| {
        row_spec(RowId::StartIn, ctx, &[], &Default::default())
            .value
            .unwrap()
    };

    // The fresh default is the list by choice, so the row reads plainly, not as a fallback.
    assert_eq!(value(&ctx), "Host list");
    assert!(adjust(RowId::StartIn, 1, false, &mut ctx));
    assert_eq!(ctx.settings.start_in, "library");
    assert_eq!(value(&ctx), "Host list (no default host)");
    store.set_known_hosts(KnownHosts {
        hosts: vec![KnownHost {
            name: "Desk".into(),
            addr: "10.0.0.5".into(),
            fp_hex: "aa".repeat(32),
            paired: true,
            ..Default::default()
        }],
    });
    assert_eq!(
        value(&ctx),
        "Library \u{b7} Desk",
        "one paired host derives"
    );

    assert!(adjust(RowId::StartIn, 1, false, &mut ctx));
    assert_eq!(ctx.settings.start_in, "stream");
    assert_eq!(value(&ctx), "Stream \u{b7} Desk");
    assert!(
        !adjust(RowId::StartIn, 1, false, &mut ctx),
        "the last value = thud"
    );
    assert!(adjust(RowId::StartIn, 1, true, &mut ctx));
    assert_eq!(ctx.settings.start_in, "hosts");
    assert_eq!(
        value(&ctx),
        "Host list",
        "a resolved default host does not dress up the list"
    );
}

/// The phone's own motor, gyro and SC2 dongle: only a console that has the phone's
/// screen offers them, so a TV and a Mac never do.
#[test]
fn the_phone_rows_need_the_phones_screen() {
    let phone_rows = [RowId::PhoneRumble, RowId::PhoneGyro, RowId::Sc2Passthrough];
    let mut settings = Settings::default();
    let library = crate::library::LibraryShared::default();
    let ctx = Ctx::test(&mut settings, &library);
    assert!(phone_rows.iter().all(|id| !row_applies(*id, &ctx)));
    let phone = crate::screens::Device {
        screen: Some(crate::shell::DeviceScreen {
            full: (2796, 1290),
            safe: (2796, 1290),
        }),
        ..crate::screens::Device::test()
    };
    let ctx = Ctx {
        device: &phone,
        ..ctx
    };
    assert!(phone_rows.iter().all(|id| row_applies(*id, &ctx)));
}

/// An OS that answers takes the row's place; no answer puts the row back.
#[test]
fn the_reduce_motion_row_steps_aside_for_the_os() {
    let _slot = crate::os_theme::REDUCE_MOTION_TEST.lock().unwrap();
    with_ctx(|ctx| {
        assert!(row_applies(RowId::ReduceMotion, ctx));
        crate::os_theme::set_os_reduce_motion(Some(false));
        assert!(!row_applies(RowId::ReduceMotion, ctx));
        crate::os_theme::set_os_reduce_motion(None);
        assert!(row_applies(RowId::ReduceMotion, ctx));
    });
}

/// The background pair shows on Android and on an Apple phone, tablet or TV, never on a
/// Mac window; the timeout only while the switch is on, and it steps through the minutes
/// the touch UIs offer.
#[test]
fn the_background_rows_follow_the_device() {
    use crate::platform::Platform;
    let mut settings = Settings::default();
    let library = crate::library::LibraryShared::default();
    let mac = crate::screens::Device {
        platform: Platform::Apple,
        ..crate::screens::Device::test()
    };
    let c = Ctx {
        device: &mac,
        ..Ctx::test(&mut settings, &library)
    };
    assert!(!row_applies(RowId::BackgroundKeepAlive, &c), "a Mac");
    let apple_tv = crate::screens::Device {
        tv: true,
        ..mac.clone()
    };
    let mut c = Ctx {
        device: &apple_tv,
        ..c
    };
    {
        let ctx = &mut c;
        assert!(row_applies(RowId::BackgroundKeepAlive, ctx), "an Apple TV");
        assert!(!row_applies(RowId::BackgroundTimeout, ctx), "switch off");
        assert!(adjust(RowId::BackgroundKeepAlive, 1, true, ctx));
        assert!(row_applies(RowId::BackgroundTimeout, ctx));
        assert_eq!(background_timeout(ctx.settings), 10);
        assert!(adjust(RowId::BackgroundTimeout, 1, false, ctx));
        assert_eq!(background_timeout(ctx.settings), 30);
        assert!(
            !adjust(RowId::BackgroundTimeout, 1, false, ctx),
            "30 is the top"
        );
    }
    let android = crate::screens::Device {
        platform: Platform::Android,
        ..crate::screens::Device::test()
    };
    let c = Ctx {
        device: &android,
        ..c
    };
    assert!(
        row_applies(RowId::BackgroundKeepAlive, &c),
        "an Android phone"
    );
}

/// The two row-to-field maps agree: a row names an overlay field exactly when an overlay
/// holding every field marks it overridden.
#[test]
fn the_preset_field_map_matches_the_override_map() {
    let every: SettingsOverlay = serde_json::from_value(serde_json::json!({
        "width": 1920, "height": 1080, "refresh_hz": 60, "match_window": false,
        "bitrate_kbps": 20000, "render_scale": 1.0, "video_fit": "fit", "codec": "hevc",
        "hdr_enabled": true, "enable_444": false, "ten_bit_sdr": false, "compositor": "auto",
        "audio_channels": 2, "audio_format": "opus", "keep_host_audio": false,
        "mic_enabled": true, "echo_cancel": true, "touch_mode": "trackpad",
        "mouse_mode": "capture", "invert_scroll": false, "inhibit_shortcuts": true,
        "gamepad": "auto", "gamepad_forwarding": true, "system_buttons": "auto",
        "guide_gesture": "auto", "stats_verbosity": "normal", "fullscreen_on_stream": true,
        "present_priority": "latency", "smooth_buffer": 0, "vsync": false, "allow_vrr": true
    }))
    .unwrap();
    for id in TABS.iter().flat_map(|(_, rows)| rows.iter().copied()) {
        assert_eq!(
            preset_field(id).is_some(),
            overrides_row(id, &every),
            "{id:?}"
        );
    }
}

/// Every row kept in `extra` reads its default unwritten, and one step writes the key it
/// reads back, so the row never shows one value and steps from another.
#[test]
fn every_extra_row_steps_the_value_it_shows() {
    let ids = TABS
        .iter()
        .flat_map(|(_, rows)| rows.iter().copied())
        .filter(|id| id.extra().is_some());
    for id in ids {
        let mut settings = Settings::default();
        let library = crate::library::LibraryShared::default();
        let mut ctx = Ctx::test(&mut settings, &library);
        let shown = |ctx: &Ctx| row_spec(id, ctx, &[], &Default::default()).value;
        let before = shown(&ctx);
        assert!(adjust(id, 1, true, &mut ctx), "{id:?} steps");
        assert_ne!(shown(&ctx), before, "{id:?} shows what it stepped to");
        let (Some(Extra::Bool(key, _)) | Some(Extra::Choice(key, _, _))) = id.extra() else {
            unreachable!()
        };
        assert!(ctx.settings.extra.contains_key(key), "{id:?} writes {key}");
    }
}

/// The TV maps Xbox 360, Steam Deck and Steam Controller 2 to Automatic, so its pad-type row
/// steps past them.
#[test]
fn the_tv_pad_type_row_offers_only_what_it_creates() {
    let library = crate::library::LibraryShared::default();
    let tv = crate::screens::Device {
        platform: crate::platform::Platform::WebOS,
        tv: true,
        ..crate::screens::Device::test()
    };
    let mut settings = Settings::default();
    let mut seen = Vec::new();
    for _ in 0..PAD_TYPES.len() {
        let mut ctx = Ctx {
            device: &tv,
            ..Ctx::test(&mut settings, &library)
        };
        adjust(RowId::PadType, 1, true, &mut ctx);
        seen.push(settings.gamepad.clone());
    }
    assert!(seen.iter().any(|v| v == "dualsense"));
    for gone in ["xbox360", "steamdeck", "steamcontroller2"] {
        assert!(!seen.iter().any(|v| v == gone), "{gone} offered on the TV");
    }
}

/// webOS answers `LoadLicenses` like every other client, so its row opens the console's screen.
#[test]
fn the_licences_row_opens_the_console_screen_on_webos() {
    let mut settings = Settings::default();
    let library = crate::library::LibraryShared::default();
    let webos = crate::screens::Device {
        platform: crate::platform::Platform::WebOS,
        ..crate::screens::Device::test()
    };
    let mut ctx = Ctx {
        device: &webos,
        ..Ctx::test(&mut settings, &library)
    };
    let mut s = SettingsScreen::with_presets(Vec::new());
    s.tab = TABS
        .iter()
        .position(|(name, _)| *name == "About")
        .expect("the About section");
    let ids = s.row_ids(&ctx);
    s.list.cursor = ids
        .iter()
        .position(|r| *r == RowId::Licenses)
        .expect("webOS lists the licences");
    let mut fx = Outbox::default();
    s.apply_row(ListMsg::Activate, None, &ids, &mut ctx, &mut fx);
    assert!(matches!(fx.nav, Some(crate::screens::Nav::Push(ref b))
        if matches!(**b, Screen::Licenses(_))));
    assert_eq!(fx.cmds, vec![crate::model::ConsoleCmd::LoadLicenses]);
}
