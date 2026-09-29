//! The CI screenshot scenes: one mock-populated scene per run, captured by the app itself.

use crate::app::AppModel;
use crate::hosts::{ConnectRequest, HostsMsg};
use gtk::glib;
use gtk::prelude::*;
use relm4::prelude::*;
use std::rc::Rc;

/// The handles `run_shot` needs — cloned out of `AppModel` before it moves into the
/// component parts, so the scene can be dispatched from the window's `map` callback.
pub struct ShotCtx {
    pub window: adw::ApplicationWindow,
    pub hosts: relm4::Sender<HostsMsg>,
    pub library: relm4::Sender<crate::library::LibraryMsg>,
    pub views: adw::ViewStack,
    pub store: Rc<crate::store::Store>,
    pub gamepad: crate::gamepad::GamepadService,
    pub identity: (String, String),
    pub sender: ComponentSender<AppModel>,
}

/// `PUNKTFUNK_SHOT_SCENE`, when set, selects a scripted host-free scene for CI screenshots.
pub fn shot_scene() -> Option<String> {
    std::env::var("PUNKTFUNK_SHOT_SCENE")
        .ok()
        .filter(|s| !s.is_empty())
}

/// Render one mock-populated, host-free scene over the already-presented window, then
/// print `PF_SHOT_READY` once it has settled. When `PUNKTFUNK_SHOT_OUT=/path.png` is set
/// the app CAPTURES ITSELF (widget snapshot → gsk render → PNG) — no Xvfb/ImageMagick
/// needed. The stream and gamepad-library scenes are gone with the pages (both live in
/// the session binary now).
pub fn run_shot(ctx: &ShotCtx, scene: &str) {
    let sender = &ctx.sender;
    // A plausible host for the trust/pair dialogs (fp_hex = 64 hex chars).
    let mock_req = || ConnectRequest {
        name: "Living Room PC".to_string(),
        addr: "192.168.1.42".to_string(),
        port: 9777,
        fp_hex: Some(
            "9f8e7d6c5b4a39281706f5e4d3c2b1a0998877665544332211ffeeddccbbaa00".to_string(),
        ),
        pair_optional: true,
        launch: None,
        mac: Vec::new(),
        preset: None,
    };
    let mock_advert =
        |key: &str, name: &str, addr: &str, fp: &str| crate::discovery::DiscoveredHost {
            key: key.to_string(),
            fullname: format!("{key}._punktfunk._udp.local."),
            name: name.to_string(),
            addr: addr.to_string(),
            port: 9777,
            fp_hex: fp.to_string(),
            pair: "required".to_string(),
            mgmt_port: None,
            mac: Vec::new(),
            os: "linux/arch/steamos".to_string(),
        };

    // What the self-capture renders: the main window, its dialogs included.
    let target: gtk::Widget = ctx.window.clone().upcast();
    let hosts = &ctx.hosts;
    match scene {
        // Saved hosts come from the seeded known-hosts store; on top, inject synthetic
        // adverts through the same path the mDNS stream feeds.
        "hosts" | "02-hosts" => {
            let _ = hosts.send(HostsMsg::Advert(mock_advert(
                "mock-online",
                "Living Room PC",
                "192.168.1.42",
                "9f8e7d6c5b4a39281706f5e4d3c2b1a0998877665544332211ffeeddccbbaa00",
            )));
            let _ = hosts.send(HostsMsg::Advert(mock_advert(
                "mock-new",
                "steamdeck",
                "192.168.1.77",
                "00aabbccddeeff112233445566778899a0b1c2d3e4f5061728394a5b6c7d8e9f",
            )));
            let _ = hosts.send(HostsMsg::Probed(mock_online(ctx)));
        }
        // The first saved host's page, its host answering.
        "host" | "09-host" => {
            let _ = hosts.send(HostsMsg::Probed(mock_online(ctx)));
            if let Some(k) = ctx.store.hosts().hosts.first() {
                let _ = hosts.send(HostsMsg::Act(crate::hosts::Act::Details(
                    crate::hosts::HostRef::of(k),
                )));
            }
        }
        "about" | "08-about" => {
            crate::settings::show_about(&ctx.window);
        }
        "settings" | "03-settings" => {
            // Mock devices so the shot shows the probe-dependent pickers populated.
            let dev = |name: &str, description: &str| pf_client_core::audio::AudioDevice {
                name: name.to_string(),
                description: description.to_string(),
            };
            let probes = crate::settings::DeviceProbes {
                adapters: vec![
                    "NVIDIA GeForce RTX 4070".to_string(),
                    "AMD Radeon 780M".to_string(),
                ],
                speakers: vec![dev("alsa_output.mock-hdmi", "HDMI / DisplayPort Audio")],
                mics: vec![dev("alsa_input.mock-usb", "USB Microphone Analog Stereo")],
            };
            // `PUNKTFUNK_SHOT_SETTINGS_SCOPE=<preset id|name>` captures the dialog in
            // PRESET scope — the second half of the settings surface (design
            // client-settings-profiles.md §5.1), where only presetable rows render.
            let scope = std::env::var("PUNKTFUNK_SHOT_SETTINGS_SCOPE")
                .ok()
                .filter(|v| !v.is_empty())
                .and_then(|reference| {
                    ctx.store
                        .presets()
                        .resolve(&reference)
                        .0
                        .map(|p| crate::settings::Scope::Preset(p.id.clone()))
                })
                .unwrap_or(crate::settings::Scope::Defaults);
            let dialog = crate::settings::show_scoped(
                &ctx.window,
                ctx.store.clone(),
                &ctx.gamepad,
                &probes,
                scope,
                |_| {},
                || {},
            );
            // Optional page for the capture (general/display/input/audio/controllers);
            // the dialog opens on General otherwise.
            if let Ok(page) = std::env::var("PUNKTFUNK_SHOT_SETTINGS_PAGE") {
                if !page.is_empty() {
                    use adw::prelude::PreferencesDialogExt as _;
                    dialog.set_visible_page_name(&page);
                }
            }
        }
        "trust" | "04-trust" => crate::app::gate::tofu_dialog(&ctx.window, sender, mock_req()),
        "pair" | "05-pair" => {
            crate::app::gate::pin_dialog(&ctx.window, sender, ctx.identity.clone(), mock_req())
        }
        "addhost" | "06-addhost" => {
            let _ = hosts.send(HostsMsg::ShowAddHost);
        }
        "shortcuts" | "07-shortcuts" => {
            adw::prelude::AdwDialogExt::present(&crate::app::shortcuts_dialog(), Some(&ctx.window));
        }
        // The Library on the first shelf with injected titles: mixed stores for the badge
        // set, a launcher, placeholders, and one solid-colour texture standing in for a poster.
        "library" | "08-library" => {
            let (games, art) = mock_library();
            ctx.views.set_visible_child_name("library");
            let _ = ctx
                .library
                .send(crate::library::LibraryMsg::Mock(games, art));
        }
        other => tracing::warn!("unknown PUNKTFUNK_SHOT_SCENE={other:?}; showing hosts only"),
    }

    let settle_ms = std::env::var("PUNKTFUNK_SHOT_SETTLE_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(900);
    let scene = scene.to_string();
    glib::timeout_add_local_once(std::time::Duration::from_millis(settle_ms), move || {
        use std::io::Write as _;
        // Self-capture of the dialog scenes (trust/pair/settings/addhost) needs a GL
        // renderer: `WidgetPaintable(window)` under the cairo software renderer doesn't
        // composite the `AdwDialog` overlay layer (the dialog IS presented — the
        // page-content scenes capture fine either way; CI uses GL or the X11 root-grab).
        let self_capture = std::env::var("PUNKTFUNK_SHOT_OUT")
            .ok()
            .filter(|p| !p.is_empty());
        if let Some(out) = &self_capture {
            if let Err(e) = save_png(&target, out) {
                eprintln!("PF_SHOT_ERROR scene={scene}: {e:#}");
            }
        }
        println!("PF_SHOT_READY scene={scene}");
        let _ = std::io::stdout().flush();
        // Self-capture mode: the shot is on disk — exit so back-to-back scene runs
        // don't stack windows on a live desktop.
        if self_capture.is_some() {
            std::process::exit(0);
        }
    });
}

/// The mock game set for the `library` scene: mixed stores exercising the badge set,
/// plus one solid-colour poster texture.
/// A probe sweep that found the first saved host, the rest asleep.
fn mock_online(ctx: &ShotCtx) -> std::collections::HashMap<String, bool> {
    ctx.store
        .hosts()
        .hosts
        .iter()
        .enumerate()
        .map(|(i, k)| (k.card_key(), i == 0))
        .collect()
}

fn mock_library() -> (
    Vec<pf_client_core::library::GameEntry>,
    Vec<(String, gtk::gdk::Texture)>,
) {
    let game =
        |id: &str, store: &str, title: &str, played: u64| pf_client_core::library::GameEntry {
            id: id.to_string(),
            store: store.to_string(),
            title: title.to_string(),
            art: pf_client_core::library::Artwork::default(),
            platform: None,
            developer: None,
            release_year: None,
            genres: Vec::new(),
            role: None,
            icon: None,
            stats: (played > 0).then_some(pf_client_core::library::GameStats {
                last_played_unix_ms: 1_700_000_000_000 + played * 3_600_000,
                play_time_ms: played * 3_600_000,
                last_run_ms: 0,
                launch_count: 1,
            }),
        };
    let games = vec![
        pf_client_core::library::GameEntry {
            role: Some("launcher".into()),
            icon: Some("steam".into()),
            ..game("steam:bigpicture", "steam", "Steam", 0)
        },
        game("steam:570", "steam", "Dota 2", 30),
        game("steam:1091500", "steam", "Cyberpunk 2077", 12),
        game("custom:emu-1", "custom", "RetroArch", 0),
        game("heroic:fortnite", "heroic", "Fortnite", 0),
        game("gog:witcher3", "gog", "The Witcher 3", 5),
        game("lutris:osu", "lutris", "osu!", 0),
        game("steam:1145360", "steam", "Hades", 0),
        game("steam:504230", "steam", "Celeste", 0),
    ];
    let art = vec![(
        "steam:570".to_string(),
        solid_texture(300, 450, 0x35, 0x84, 0xe4),
    )];
    (games, art)
}

/// A WxH single-colour RGBA texture — the `library` scene's stand-in for a fetched poster.
fn solid_texture(w: i32, h: i32, r: u8, g: u8, b: u8) -> gtk::gdk::Texture {
    let px = [r, g, b, 0xff].repeat((w * h) as usize);
    gtk::gdk::MemoryTexture::new(
        w,
        h,
        gtk::gdk::MemoryFormat::R8g8b8a8,
        &glib::Bytes::from_owned(px),
        (w * 4) as usize,
    )
    .upcast()
}

/// Snapshot `widget` (the whole window, dialogs included) into a PNG: WidgetPaintable →
/// `gtk::Snapshot` → the realized native's gsk renderer → `GdkTexture::save_to_png`.
fn save_png(widget: &gtk::Widget, path: &str) -> anyhow::Result<()> {
    use anyhow::Context as _;
    let (w, h) = (widget.width(), widget.height());
    anyhow::ensure!(w > 0 && h > 0, "widget not laid out yet ({w}x{h})");
    let paintable = gtk::WidgetPaintable::new(Some(widget));
    let snapshot = gtk::Snapshot::new();
    paintable.snapshot(&snapshot, f64::from(w), f64::from(h));
    let node = snapshot.to_node().context("empty snapshot")?;
    let renderer = widget
        .native()
        .context("widget not realized")?
        .renderer()
        .context("no gsk renderer")?;
    let texture = renderer.render_texture(node, None);
    texture
        .save_to_png(path)
        .with_context(|| format!("save {path}"))?;
    Ok(())
}
