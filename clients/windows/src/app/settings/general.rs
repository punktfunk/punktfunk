//! General: session and statistics, plus the device-wide rows a preset never carries.

use super::{
    advanced_group, described_labeled, described_overridable, group, presets, setting_combo,
    setting_toggle, Cx,
};
use crate::trust::{KnownHosts, Settings};
use pf_client_core::start;
use pf_client_core::trust::{HudCorner, StatsVerbosity};
use punktfunk_core::hud::{stats_scale, STATS_SCALE_PCTS};
use windows_reactor::*;

/// Stats-overlay tiers: `(stored value, display label)` — the cross-client verbosity ladder
/// (Compact ⊂ Normal ⊂ Detailed); Ctrl+Alt+Shift+S cycles it live in the session window.
const STATS_TIERS: &[(StatsVerbosity, &str)] = &[
    (StatsVerbosity::Off, "Off"),
    (StatsVerbosity::Compact, "Compact"),
    (StatsVerbosity::Normal, "Normal"),
    (StatsVerbosity::Detailed, "Detailed"),
];

/// Names the host the Start in row resolves to, and says when it resolves to nothing — which
/// is what every value does until one host is paired. The pointer is written from a host's own
/// tile menu, not from this page, so the help line is where the two meet.
fn start_in_help() -> String {
    let known = KnownHosts::load();
    match start::default_host(&Settings::load(), &known) {
        Some(i) => format!(
            "Library opens {}\u{2019}s games; Stream also connects to its desktop. Back leaves \
             either one on the host list.",
            known.hosts[i].name
        ),
        None => "Opens on the host list: there is no default host yet. Pair one, or pick one \
                 from a host\u{2019}s menu when several are paired."
            .into(),
    }
}

/// General: session, statistics.
pub(super) fn general_section(cx: &Cx) -> Vec<Element> {
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

    let (hud_names, hud_i) = presets(STATS_TIERS, |v| *v == s.stats_verbosity());
    let hud_combo = setting_combo(cx, scope, "stats_verbosity", hud_names, hud_i, |s, i| {
        s.set_stats_verbosity(STATS_TIERS[i].0);
    });
    let advanced_toggle = setting_toggle(cx, scope, "advanced_stats", s.advanced_stats, |s, on| {
        s.advanced_stats = on
    });
    // Explorer hands a URL to the default browser; best-effort, like About's log folder.
    let stats_docs_button = button("What each number means").on_click(|| {
        let _ = std::process::Command::new("explorer.exe")
            .arg("https://docs.punktfunk.unom.io/docs/stats")
            .spawn();
    });

    let mut out = group(
        Some("Session"),
        vec![described_overridable(
            cx,
            "fullscreen_on_stream",
            "Start streams fullscreen",
            fullscreen_toggle,
            "Go fullscreen when a session starts; F11 or Alt+Enter switches back \
                 live.",
        )]
        .into_iter()
        // Auto-wake is about this host and this network, not about "Game vs Work" —
        // it stays global in v1 (design §3, tier H/G).
        .chain((!preset_mode).then(|| {
            described_labeled(
                "Auto-wake on connect",
                auto_wake_toggle,
                "Connecting to a saved host that\u{2019}s offline sends Wake-on-LAN and \
                 waits for it to boot. Turn off if hosts behind a VPN look offline when \
                 they aren\u{2019}t.",
            )
        }))
        .chain(
            (!preset_mode).then(|| described_labeled("Start in", start_in_combo, &start_in_help())),
        )
        .collect(),
        None,
    );
    let mut stats_rows = vec![described_overridable(
        cx,
        "stats_verbosity",
        "Statistics overlay",
        hud_combo,
        "Live session stats in a corner overlay \u{2014} Compact is a one-line pill, \
         Detailed adds the stage breakdown. Ctrl+Alt+Shift+S cycles the tiers any time.",
    )];
    if !preset_mode {
        stats_rows.push(stats_docs_button.into());
    }
    out.extend(group(Some("Statistics"), stats_rows, None));
    // Device-wide, and shown in both scopes: it changes what this page lists, not a stream.
    let show_toggle = setting_toggle(cx, "", "show_advanced", s.show_advanced, |s, on| {
        s.show_advanced = on
    });
    out.extend(group(
        None,
        vec![described_labeled(
            "Show advanced",
            show_toggle,
            "Adds the settings most players never need to change.",
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
            STATS_SCALE_PCTS.iter().map(|p| format!("{p} %")).collect(),
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
                    "Advanced statistics",
                    advanced_toggle,
                    "Off shows the figures Moonlight's overlay also shows. On shows capture \
                     to glass as p50/p95 and every stage between.",
                ),
                described_labeled(
                    "Statistics position",
                    corner_combo,
                    "The corner the statistics overlay sits in.",
                ),
                described_labeled(
                    "Statistics size",
                    size_combo,
                    "The overlay's size, on top of your display's scaling.",
                ),
                described_labeled(
                    "Exit hint",
                    hint_toggle,
                    "Shows how to leave for a few seconds when a stream starts.",
                ),
            ],
            changed,
            false,
        ));
    }
    out
}
