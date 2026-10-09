//! The physical head a gamescope DRM session drives.
//!
//! Nested and headless gamescopes have no connector of their own. A session that is the DRM
//! master does: this module finds that connector from `/proc/<pid>/cmdline` (backend flag and
//! `--prefer-output`) and `/sys/class/drm` (plug status + EDID). gamescope has no protocol that
//! reports the output it lights; [`gamescope_control`] only sets things.
//!
//! Only the driven head is listed. Mirror capture attaches to gamescope's composited PipeWire
//! node (`vdisplay::mirror`), so an undriven connector cannot be streamed. The pin id is the
//! connector name (`HDMI-A-1`), the same string `PUNKTFUNK_CAPTURE_MONITOR` names.
//!
//! Empty, never an error, when there is no DRM session or nothing is plugged in — same contract
//! as [`crate::monitors::list`]. See `design/per-monitor-portal-capture.md`.

use super::argv::{flag_value, gamescope_argvs, gamescope_output_size};
use crate::monitors::{describe, PhysicalMonitor};
use std::path::Path;

/// The DRM-driven head, or empty. Empty is not an error: no gamescope, a nested/headless
/// backend, or nothing plugged in. Same contract as [`crate::monitors::list`].
pub(crate) fn list_monitors() -> anyhow::Result<Vec<PhysicalMonitor>> {
    Ok(heads_under(Path::new("/sys/class/drm"), &gamescope_argvs()))
}

/// [`list_monitors`] against a sysfs root and argv set.
///
/// Size is `-W`/`-H` on the argv selected here — what the capture node produces — not the EDID
/// preferred timing, and not another gamescope on the box (a nested child, or a headless one
/// this crate spawned).
fn heads_under(base: &Path, argvs: &[Vec<String>]) -> Vec<PhysicalMonitor> {
    // First DRM-backed argv, not first argv: a nested child must not mask its parent.
    let Some(argv) = argvs.iter().find(|a| drives_drm(a)) else {
        return Vec::new();
    };
    let output_size = gamescope_output_size(argv);
    let connected = connected_connectors(base);
    if connected.is_empty() {
        return Vec::new();
    }
    let driven = resolve_driven(&connected, prefer_output(argv));
    driven
        .into_iter()
        .map(|c| {
            let edid = std::fs::read(base.join(&c.dir).join("edid"))
                .ok()
                .and_then(|b| parse_edid(&b));
            let (make, model) = edid
                .as_ref()
                .map(|e| (e.make.clone(), e.model.clone()))
                .unwrap_or_default();
            // Size: `-W`/`-H` (capture node). Refresh: EDID only — `-r` is the nested
            // composite rate, not the connector; using it caps a 120 Hz panel at launch rate.
            let (w, h) = output_size
                .or_else(|| edid.as_ref().map(|e| (e.width, e.height)))
                .or_else(|| preferred_sysfs_mode(base, &c.dir))
                .unwrap_or((0, 0));
            PhysicalMonitor {
                description: describe(&make, &model, &c.connector),
                connector: c.connector,
                width: w,
                height: h,
                refresh_mhz: edid.as_ref().map(|e| e.refresh_mhz).unwrap_or(0),
                // No compositor global space — gamescope composites one node.
                // `monitors`' origin identity-key degenerates; every row is (0, 0).
                x: 0,
                y: 0,
                scale: 1.0,
                primary: true,
                enabled: c.enabled,
                // Headless gamescopes this crate spawns never reach here (`drives_drm`).
                managed: false,
            }
        })
        .collect()
}

struct Connector {
    /// Sysfs directory — `card1-HDMI-A-1`.
    dir: String,
    /// Connector with `cardN-` stripped — `HDMI-A-1`, the pin id.
    connector: String,
    enabled: bool,
}

/// Plugged-in connectors, sorted by name (`read_dir` order is not stable).
fn connected_connectors(base: &Path) -> Vec<Connector> {
    let Ok(entries) = std::fs::read_dir(base) else {
        return Vec::new();
    };
    let mut out: Vec<Connector> = entries
        .flatten()
        .filter_map(|e| {
            let dir = e.file_name().to_str()?.to_string();
            // `cardN-<connector>`. `card1`/`renderD128` have no dash; the connector name does.
            let (card, connector) = dir.split_once('-')?;
            if !card.starts_with("card") || !card[4..].bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            let status = std::fs::read_to_string(e.path().join("status")).ok()?;
            if status.trim() != "connected" {
                return None;
            }
            let enabled = std::fs::read_to_string(e.path().join("enabled"))
                .map(|s| s.trim() == "enabled")
                .unwrap_or(true);
            Some(Connector {
                connector: connector.to_string(),
                dir,
                enabled,
            })
        })
        .collect();
    out.sort_by(|a, b| a.connector.cmp(&b.connector));
    out
}

/// Connector gamescope is lighting.
///
/// `--prefer-output` is a comma-separated preference list; `*` means "any" and is skipped, not
/// matched. One connected head is unambiguous. Otherwise every connected head is returned —
/// not a guess: the stream is the composited node either way, so a wrong label still streams
/// the right pixels.
fn resolve_driven(connected: &[Connector], prefer: Option<&str>) -> Vec<Connector> {
    let clone = |c: &Connector| Connector {
        dir: c.dir.clone(),
        connector: c.connector.clone(),
        enabled: c.enabled,
    };
    if let Some(list) = prefer {
        for want in list.split(',').map(str::trim).filter(|w| !w.is_empty()) {
            if want == "*" {
                continue;
            }
            if let Some(c) = connected
                .iter()
                .find(|c| c.connector.eq_ignore_ascii_case(want))
            {
                return vec![clone(c)];
            }
        }
    }
    if connected.len() == 1 {
        return vec![clone(&connected[0])];
    }
    tracing::debug!(
        heads = connected.len(),
        "gamescope: --prefer-output does not name a connected head — listing all of them (the \
         mirror attaches to the composited node either way)"
    );
    connected.iter().map(clone).collect()
}

/// DRM backend — owns a physical connector.
///
/// No `--backend` means DRM (gamescope's default). `headless` is what this crate spawns;
/// `wayland`/`sdl`/`x11` are nested. `-b` is the short form.
fn drives_drm(argv: &[String]) -> bool {
    match backend_flag(argv) {
        Some(b) => b.eq_ignore_ascii_case("drm"),
        None => true,
    }
}

fn backend_flag(argv: &[String]) -> Option<&str> {
    flag_value(argv, &["--backend", "-b"])
}

fn prefer_output(argv: &[String]) -> Option<&str> {
    flag_value(argv, &["--prefer-output", "-O"])
}

/// Sysfs `modes` first line (`WIDTHxHEIGHT`) — last-resort size after `-W`/`-H` and EDID.
fn preferred_sysfs_mode(base: &Path, dir: &str) -> Option<(u32, u32)> {
    let modes = std::fs::read_to_string(base.join(dir).join("modes")).ok()?;
    let first = modes.lines().next()?.trim();
    let (w, h) = first.split_once('x')?;
    Some((w.parse().ok()?, h.trim().parse().ok()?))
}

struct Edid {
    make: String,
    model: String,
    width: u32,
    height: u32,
    refresh_mhz: u32,
}

/// Manufacturer id, monitor-name descriptor, preferred detailed timing.
///
/// First 128-byte block only. CTA-861 extensions add modes, not a better name or preferred
/// timing — all a picker row needs.
fn parse_edid(bytes: &[u8]) -> Option<Edid> {
    if bytes.len() < 128 || bytes[..8] != [0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00] {
        return None;
    }
    // Three 5-bit letters packed BE, 1 = 'A' (bit 15 reserved). PnP id, not a marketing
    // name; `describe` pairs it with the model string.
    let m = u16::from_be_bytes([bytes[8], bytes[9]]);
    let letter = |shift: u16| -> Option<char> {
        let v = ((m >> shift) & 0x1F) as u8;
        (1..=26).contains(&v).then(|| (b'A' + v - 1) as char)
    };
    let make: String = match (letter(10), letter(5), letter(0)) {
        (Some(a), Some(b), Some(c)) => [a, b, c].iter().collect(),
        // Zeroed/garbage id: leave empty so `describe` falls back to the connector.
        _ => String::new(),
    };
    let mut model = String::new();
    let mut timing: Option<(u32, u32, u32)> = None;
    // Four 18-byte descriptors. First timing is preferred; pixel clock 0 marks a tagged
    // text block, not a mode.
    for off in [54usize, 72, 90, 108] {
        let d = &bytes[off..off + 18];
        let pixel_clock = u16::from_le_bytes([d[0], d[1]]);
        if pixel_clock == 0 {
            // 0xFC = monitor name, up to 13 bytes, 0x0A-terminated and space-padded.
            if d[3] == 0xFC && model.is_empty() {
                let raw = &d[5..18];
                let end = raw.iter().position(|&b| b == 0x0A).unwrap_or(raw.len());
                model = String::from_utf8_lossy(&raw[..end]).trim().to_string();
            }
            continue;
        }
        if timing.is_some() {
            continue;
        }
        // Split-nibble: high 4 of the upper byte extend hactive, low 4 hblank
        // (likewise vactive/vblank).
        let hactive = d[2] as u32 | ((d[4] as u32 & 0xF0) << 4);
        let hblank = d[3] as u32 | ((d[4] as u32 & 0x0F) << 8);
        let vactive = d[5] as u32 | ((d[7] as u32 & 0xF0) << 4);
        let vblank = d[6] as u32 | ((d[7] as u32 & 0x0F) << 8);
        let (htotal, vtotal) = (hactive + hblank, vactive + vblank);
        if hactive == 0 || vactive == 0 || htotal == 0 || vtotal == 0 {
            continue;
        }
        // Pixel clock is 10 kHz units; ×10_000_000 → mHz. u64 so a 4K htotal×vtotal
        // cannot overflow the intermediate.
        let refresh_mhz =
            ((pixel_clock as u64 * 10_000_000) / (htotal as u64 * vtotal as u64)) as u32;
        timing = Some((hactive, vactive, refresh_mhz));
    }
    let (width, height, refresh_mhz) = timing.unwrap_or((0, 0, 0));
    Some(Edid {
        make,
        model,
        width,
        height,
        refresh_mhz,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    /// Fake `/sys/class/drm` including `card1`/`renderD128`, so the filter sees real noise.
    fn sysfs(name: &str, connectors: &[(&str, &str, &str)]) -> std::path::PathBuf {
        let base = std::env::temp_dir().join(format!("pf-gs-heads-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        for n in ["card1", "renderD128"] {
            std::fs::create_dir_all(base.join(n)).unwrap();
        }
        for (dir, status, enabled) in connectors {
            let d = base.join(dir);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("status"), status).unwrap();
            std::fs::write(d.join("enabled"), enabled).unwrap();
        }
        base
    }

    #[test]
    fn a_drm_backed_session_reports_the_head_it_drives() {
        let base = sysfs(
            "drm",
            &[
                ("card1-HDMI-A-1", "connected\n", "enabled\n"),
                ("card1-DP-1", "disconnected\n", "disabled\n"),
            ],
        );
        let heads = heads_under(
            &base,
            &[argv("/usr/bin/gamescope --prefer-output HDMI-A-1 --steam")],
        );
        assert_eq!(heads.len(), 1);
        assert_eq!(heads[0].connector, "HDMI-A-1");
        assert!(heads[0].enabled && heads[0].primary && !heads[0].managed);
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn a_headless_or_nested_session_reports_nothing() {
        let base = sysfs(
            "headless",
            &[("card1-HDMI-A-1", "connected\n", "enabled\n")],
        );
        for a in [
            "gamescope --backend headless -W 1920 -H 1080",
            "gamescope --backend=headless",
            "gamescope -b wayland",
            "gamescope --backend sdl",
        ] {
            assert!(
                heads_under(&base, &[argv(a)]).is_empty(),
                "expected no heads for {a:?}"
            );
        }
        assert!(heads_under(&base, &[]).is_empty());
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn a_nested_child_does_not_disqualify_its_drm_parent() {
        let base = sysfs("nested", &[("card1-eDP-1", "connected\n", "enabled\n")]);
        let heads = heads_under(
            &base,
            &[
                argv("gamescope --backend wayland -W 1280 -H 800"),
                argv("/usr/bin/gamescope --prefer-output *,eDP-1 -W 2560 -H 1440 --steam"),
            ],
        );
        assert_eq!(heads.len(), 1);
        assert_eq!(heads[0].connector, "eDP-1");
        // Size from the DRM parent, not the nested child listed first.
        assert_eq!((heads[0].width, heads[0].height), (2560, 1440));
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn prefer_output_skips_the_wildcard_and_picks_the_named_head() {
        let base = sysfs(
            "wildcard",
            &[
                ("card1-eDP-1", "connected\n", "enabled\n"),
                ("card1-HDMI-A-1", "connected\n", "enabled\n"),
            ],
        );
        let heads = heads_under(&base, &[argv("gamescope --prefer-output *,eDP-1")]);
        assert_eq!(heads.len(), 1);
        assert_eq!(heads[0].connector, "eDP-1");
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn an_undecidable_multi_head_box_lists_every_connected_head() {
        let base = sysfs(
            "ambiguous",
            &[
                ("card1-eDP-1", "connected\n", "enabled\n"),
                ("card1-HDMI-A-1", "connected\n", "enabled\n"),
            ],
        );
        let heads = heads_under(&base, &[argv("gamescope --steam")]);
        assert_eq!(
            heads
                .iter()
                .map(|h| h.connector.as_str())
                .collect::<Vec<_>>(),
            ["HDMI-A-1", "eDP-1"],
            "sorted, so the picker order is stable"
        );
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn a_drm_session_with_nothing_plugged_in_reports_nothing() {
        let base = sysfs(
            "unplugged",
            &[("card1-HDMI-A-1", "disconnected\n", "disabled\n")],
        );
        assert!(heads_under(&base, &[argv("gamescope --steam")]).is_empty());
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn the_capture_size_outranks_the_panels_preferred_timing() {
        let base = sysfs("size", &[("card1-HDMI-A-1", "connected\n", "enabled\n")]);
        let heads = heads_under(
            &base,
            &[argv("gamescope -W 2560 -H 1440 --prefer-output HDMI-A-1")],
        );
        assert_eq!((heads[0].width, heads[0].height), (2560, 1440));
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// 1920×1080 @ 60 Hz, make `ACM`, name "TEST PANEL".
    fn edid_1080p60() -> Vec<u8> {
        let mut e = vec![0u8; 128];
        e[..8].copy_from_slice(&[0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00]);
        // 'A'=1, 'C'=3, 'M'=13 → (1<<10)|(3<<5)|13
        let id: u16 = (1 << 10) | (3 << 5) | 13;
        e[8..10].copy_from_slice(&id.to_be_bytes());
        // Preferred detailed timing: 148.5 MHz = 14850 (10 kHz units); 1920+280 x 1080+45.
        e[54..56].copy_from_slice(&14850u16.to_le_bytes());
        e[56] = 1920u32 as u8;
        e[57] = 280u32 as u8;
        e[58] = ((1920 >> 4) as u8 & 0xF0) | ((280 >> 8) as u8 & 0x0F);
        e[59] = (1080 & 0xFF) as u8;
        e[60] = 45;
        e[61] = ((1080 >> 4) as u8 & 0xF0) | ((45 >> 8) as u8 & 0x0F);
        // Monitor-name descriptor (tag 0xFC).
        e[72..74].copy_from_slice(&0u16.to_le_bytes());
        e[75] = 0xFC;
        let name = b"TEST PANEL\x0a";
        e[77..77 + name.len()].copy_from_slice(name);
        e
    }

    #[test]
    fn edid_yields_the_label_and_the_preferred_timing() {
        let e = parse_edid(&edid_1080p60()).expect("valid EDID");
        assert_eq!(e.make, "ACM");
        assert_eq!(e.model, "TEST PANEL");
        assert_eq!((e.width, e.height), (1920, 1080));
        // 148_500_000 / (2200 * 1125) = 60.0 Hz
        assert_eq!(e.refresh_mhz, 60_000);
        assert_eq!(describe(&e.make, &e.model, "HDMI-A-1"), "ACM TEST PANEL");
    }

    #[test]
    fn a_bad_edid_is_rejected_rather_than_half_parsed() {
        assert!(parse_edid(&[0u8; 128]).is_none(), "no magic header");
        assert!(parse_edid(&[0u8; 12]).is_none(), "too short");
    }

    #[test]
    fn refresh_comes_from_the_panel_not_from_the_nested_rate() {
        let base = sysfs("refresh", &[("card1-HDMI-A-1", "connected\n", "enabled\n")]);
        std::fs::write(base.join("card1-HDMI-A-1").join("edid"), edid_1080p60()).unwrap();
        let heads = heads_under(
            &base,
            &[argv(
                "gamescope --nested-refresh 30 --prefer-output HDMI-A-1",
            )],
        );
        assert_eq!(heads[0].refresh_mhz, 60_000);
        assert_eq!(heads[0].mode_label(), "1920x1080@60");
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn flag_values_parse_in_both_spellings() {
        assert_eq!(
            backend_flag(&argv("gamescope --backend headless")),
            Some("headless")
        );
        assert_eq!(backend_flag(&argv("gamescope --backend=drm")), Some("drm"));
        assert_eq!(backend_flag(&argv("gamescope -b sdl")), Some("sdl"));
        assert_eq!(backend_flag(&argv("gamescope --steam")), None);
        assert_eq!(prefer_output(&argv("gamescope -O DP-2")), Some("DP-2"));
    }

    #[test]
    fn sysfs_modes_is_the_last_resort_size() {
        let base = sysfs("modes", &[("card1-HDMI-A-1", "connected\n", "enabled\n")]);
        std::fs::write(
            base.join("card1-HDMI-A-1").join("modes"),
            "3840x2160\n1920x1080\n",
        )
        .unwrap();
        let heads = heads_under(&base, &[argv("gamescope --steam")]);
        assert_eq!((heads[0].width, heads[0].height), (3840, 2160));
        std::fs::remove_dir_all(&base).unwrap();
    }
}
