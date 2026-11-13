//! General: session, console and statistics, plus the device-wide rows a preset never carries
//! and the reset that ends the page.

use super::{
    advanced_group, commit, described_labeled, described_overridable, group, presets, rows,
    setting_combo, setting_toggle, Cx,
};
use crate::app::style::*;
use crate::trust::{KnownHosts, Settings};
use pf_client_core::settings::GamepadUi;
use pf_client_core::shortcuts::Press;
use pf_client_core::start;
use pf_client_core::trust::{HudCorner, StatsVerbosity};
use pf_l10n::{client, common, desktop};
use punktfunk_core::hud::{stats_scale, STATS_SCALE_PCTS};
use windows_reactor::*;

/// Names the host the Start in row resolves to, and says when it resolves to nothing — which
/// is what every value does until one host is paired. The pointer is written from a host's own
/// tile menu, not from this page, so the help line is where the two meet.
fn start_in_help() -> String {
    let known = KnownHosts::load();
    match start::default_host(&Settings::load(), &known) {
        Some(i) => client::player_start_in_host_hint(&known.hosts[i].name),
        None => client::player_start_in_no_host_hint().into(),
    }
}

/// General: session, console, statistics.
pub(super) fn general_section(cx: &Cx) -> Vec<View> {
    let Cx {
        scope,
        ref s,
        preset_mode,
        ..
    } = *cx;
    let auto_wake_toggle = setting_toggle(cx, scope, "auto_wake", s.auto_wake, |s, on| {
        s.auto_wake = on
    });
    // Where a bare launch opens. A device preference like auto-wake beside it: which host this
    // machine opens on says nothing about how a stream should look, so it is never presetable.
    let start_in_combo = {
        let want = start::StartIn::parse(&s.start_in);
        let names = start::StartIn::ALL
            .iter()
            .map(|v| v.label().to_string())
            .collect();
        let current = start::StartIn::ALL
            .iter()
            .position(|v| *v == want)
            .unwrap_or(1);
        setting_combo(cx, scope, "start_in", names, current, |s, i| {
            s.start_in = start::StartIn::ALL[i].as_str().to_string();
        })
    };
    let fullscreen_toggle = setting_toggle(
        cx,
        scope,
        "fullscreen_on_stream",
        s.fullscreen_on_stream,
        |s, on| s.fullscreen_on_stream = on,
    );

    // The cross-client verbosity ladder (Compact ⊂ Normal ⊂ Detailed).
    let tiers: Vec<(StatsVerbosity, &str)> = StatsVerbosity::ALL
        .iter()
        .map(|v| (*v, v.label()))
        .collect();
    let (hud_names, hud_i) = presets(&tiers, |v| *v == s.stats_verbosity());
    let hud_combo = setting_combo(cx, scope, "stats_verbosity", hud_names, hud_i, |s, i| {
        s.set_stats_verbosity(StatsVerbosity::ALL[i]);
    });
    let advanced_toggle = setting_toggle(cx, scope, "advanced_stats", s.advanced_stats, |s, on| {
        s.advanced_stats = on
    });
    // Explorer hands a URL to the default browser; best-effort, like About's log folder.
    let stats_docs_button = button(client::player_stats_docs_link()).on_click(|| {
        let _ = std::process::Command::new("explorer.exe")
            .arg("https://docs.punktfunk.unom.io/docs/stats")
            .spawn();
    });

    let mut out = group(
        cx.pal,
        Some(client::prefs_group_session()),
        vec![described_overridable(
            cx,
            &rows::FULLSCREEN_ON_STREAM,
            fullscreen_toggle,
            &format!(
                "{} {}",
                client::player_fullscreen_on_stream_hint(),
                desktop::desktop_fullscreen_exit_hint(Press::Either("F11", "Alt+Enter")),
            ),
        )]
        .into_iter()
        // Auto-wake is about this host and this network, not about "Game vs Work" —
        // it stays global in v1 (design §3, tier H/G).
        .chain((!preset_mode).then(|| {
            described_labeled(
                cx.pal,
                rows::AUTO_WAKE.label(),
                auto_wake_toggle,
                client::player_auto_wake_hint(),
            )
        }))
        .chain((!preset_mode).then(|| {
            described_labeled(
                cx.pal,
                rows::START_IN.label(),
                start_in_combo,
                &start_in_help(),
            )
        }))
        .collect(),
        None,
    );
    // Which interface this device opens: the device's, never a preset's, and only where the
    // session ships its console.
    if !preset_mode && crate::app::hosts::CONSOLE_UI_AVAILABLE {
        let ui = s.gamepad_ui();
        let on_toggle = setting_toggle(
            cx,
            scope,
            rows::GAMEPAD_UI.key,
            ui != GamepadUi::Off,
            |s, on| s.set_gamepad_ui_enabled(on),
        );
        let mode_combo = setting_combo(
            cx,
            scope,
            rows::GAMEPAD_UI_MODE.key,
            vec![
                client::player_gamepad_ui_mode_connected().into(),
                client::player_gamepad_ui_mode_always().into(),
            ],
            usize::from(s.gamepad_ui_always()),
            |s, i| s.set_gamepad_ui_always(i == 1),
        )
        .is_enabled(ui != GamepadUi::Off);
        out.extend(group(
            cx.pal,
            Some(client::prefs_group_console()),
            vec![
                described_labeled(
                    cx.pal,
                    rows::GAMEPAD_UI.label(),
                    on_toggle,
                    client::player_gamepad_ui_enabled_hint(),
                ),
                described_labeled(
                    cx.pal,
                    rows::GAMEPAD_UI_MODE.label(),
                    mode_combo,
                    client::player_gamepad_ui_mode_hint(),
                ),
            ],
            None,
        ));
    }
    let mut stats_rows = vec![described_overridable(
        cx,
        &rows::STATS,
        hud_combo,
        &format!(
            "{} {}",
            client::player_stats_verbosity_hint(),
            client::player_stats_verbosity_cycle_hint("Ctrl+Alt+Shift+S"),
        ),
    )];
    if !preset_mode {
        stats_rows.push(stats_docs_button.into());
    }
    out.extend(group(
        cx.pal,
        Some(client::prefs_group_statistics()),
        stats_rows,
        None,
    ));
    // Device-wide, and shown in both scopes: it changes what this page lists, not a stream.
    let show_toggle = setting_toggle(cx, "", "show_advanced", s.show_advanced, |s, on| {
        s.show_advanced = on
    });
    out.extend(group(
        cx.pal,
        None,
        vec![described_labeled(
            cx.pal,
            rows::SHOW_ADVANCED.label(),
            show_toggle,
            client::player_show_advanced_hint(),
        )],
        None,
    ));
    // Device-wide rows: a preset never carries them.
    if !preset_mode {
        let corner = s.hud_corner(HudCorner::TopLeft);
        let corner_combo = setting_combo(
            cx,
            scope,
            "hud_placement",
            HudCorner::ALL
                .iter()
                .map(|c| c.label().to_string())
                .collect(),
            HudCorner::ALL
                .iter()
                .position(|c| *c == corner)
                .unwrap_or(0),
            |s, i| s.hud_placement = HudCorner::ALL[i].as_name().into(),
        );
        let pct = (stats_scale(s.stats_scale_pct) * 100.0).round() as u16;
        let size_combo = setting_combo(
            cx,
            scope,
            "stats_scale_pct",
            STATS_SCALE_PCTS
                .iter()
                .map(common::common_percent)
                .collect(),
            STATS_SCALE_PCTS.iter().position(|p| *p == pct).unwrap_or(1),
            |s, i| s.stats_scale_pct = STATS_SCALE_PCTS[i],
        );
        let hint_toggle = setting_toggle(cx, scope, "exit_hint", s.exit_hint, |s, on| {
            s.exit_hint = on
        });
        let changed = usize::from(s.advanced_stats)
            + usize::from(corner != HudCorner::TopLeft)
            + usize::from(pct != 100)
            + usize::from(!s.exit_hint);
        out.extend(advanced_group(
            cx,
            vec![
                described_labeled(
                    cx.pal,
                    rows::ADVANCED_STATS.label(),
                    advanced_toggle,
                    client::player_advanced_stats_hint(),
                ),
                described_labeled(
                    cx.pal,
                    rows::STATS_POSITION.label(),
                    corner_combo,
                    client::player_hud_placement_hint(),
                ),
                described_labeled(
                    cx.pal,
                    rows::STATS_SIZE.label(),
                    size_combo,
                    client::player_stats_scale_pct_hint(),
                ),
                described_labeled(
                    cx.pal,
                    rows::EXIT_HINT.label(),
                    hint_toggle,
                    client::player_exit_hint_hint(),
                ),
            ],
            changed,
            false,
        ));
        // The page's last row. A preset resets row by row, so only Default settings has it.
        let set_reset = cx.set_reset.clone();
        let reset = button(client::prefs_reset_all()).on_click(move || set_reset.call(true));
        out.extend(group(
            cx.pal,
            None,
            vec![described_labeled(
                cx.pal,
                common::common_reset(),
                reset,
                client::prefs_reset_all_hint(),
            )],
            None,
        ));
    }
    out
}

/// Asks before a reset, which puts every setting back to a fresh install's value.
pub(super) fn reset_dialog(cx: &Cx, open: bool) -> ContentDialog {
    let (ctx, rev, set_rev, set_status) = (
        cx.ctx.clone(),
        cx.rev,
        cx.set_rev.clone(),
        cx.set_status.clone(),
    );
    let (set_reset, set_custom_res, set_custom_scale) = (
        cx.set_reset.clone(),
        cx.set_custom_res.clone(),
        cx.set_custom_scale.clone(),
    );
    ContentDialog::new()
        .title(client::prefs_reset_all_title())
        .content(client::prefs_reset_all_body())
        .primary_button_text(common::common_reset())
        .close_button_text(common::common_cancel())
        .is_open(open)
        .on_closed(move |r: ContentDialogResult| {
            set_reset.call(false);
            if r != ContentDialogResult::Primary {
                return;
            }
            set_custom_res.call(false);
            set_custom_scale.call(false);
            commit(
                &ctx,
                "",
                "",
                (rev, &set_rev),
                &set_status,
                Settings::reset_to_defaults,
            );
        })
}
