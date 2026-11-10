//! Display: resolution, picture, decoding, presentation and host output.

use super::{
    advanced_group, commit, described_labeled, described_overridable, group, labeled, presets,
    rows, setting_combo, setting_toggle, Cx,
};
use crate::app::style::*;
use crate::trust::Settings;
use pf_client_core::settings::Label;
use pf_fluent::Token;
use pf_l10n::{client, common, desktop};
use windows_reactor::*;

/// Sizes by family; the Resolution combo lists one family at a time behind the Aspect combo.
/// `(0, 0)` = the native size of the display the window is on, resolved at connect.
use punktfunk_core::resolutions::{aspect_of, nearest, ASPECTS};
/// `0` = the display's native refresh, resolved at connect.
const REFRESH: &[u32] = &[0, 30, 60, 90, 120, 144, 165, 240];
/// Render-scale multipliers. `1.0` = Native; applied at connect and each match-window resize.
use punktfunk_core::render_scale::PRESETS as RENDER_SCALES;

/// A compact label for a render-scale multiplier: "Native" / "1.5×" / "2× (supersample)".
fn render_scale_label(scale: f64) -> String {
    if scale == 1.0 {
        client::player_render_scale_native().to_string()
    } else if scale > 1.0 {
        client::player_render_scale_supersample(scale)
    } else {
        format!("{scale}\u{00D7}")
    }
}
/// Decode backend presets: `(stored value, label)`. A label is a message, read when the page
/// renders; a format or product name is a literal.
// A stored legacy value that matches no preset (the D3D11VA-era "hardware", and since M10
// the bare "vulkan"/"d3d11va" that named libavcodec's rungs) shows as Automatic — which is
// how the session's ladder reads "hardware", and near enough for the other two, which
// `pf_client_core::video::migrate_decoder_pref` maps onto the entries below anyway.
const DECODERS: &[(&str, Label)] = &[
    ("auto", client::player_decoder_auto),
    ("native-vulkan", || "Vulkan Video"),
    ("native-d3d11va", || "Direct3D 11"),
    ("software", client::player_decoder_software),
];
/// Preferred-codec presets: `(stored value, display label)`. Soft — the host falls back if it
/// can't encode the chosen codec.
const CODECS: &[(&str, Label)] = &[
    ("auto", client::player_codec_auto),
    ("hevc", || "HEVC"),
    ("h264", || "H.264"),
    ("av1", || "AV1"),
    // Preference-only by design: `resolve_codec` never auto-picks PyroWave, and asking for
    // it on a host or device that can't do it simply falls back down the ladder to HEVC.
    ("pyrowave", client::player_codec_pyrowave),
];
/// `video_fit`: `(stored value, display label)`. Unknown values show as Fit.
const VIDEO_FITS: &[(&str, Label)] = &[
    ("fit", client::player_video_fit_fit),
    ("crop", client::player_video_fit_crop),
    ("stretch", client::player_video_fit_stretch),
];
/// `screens`: `(stored value, display label)`. Unknown values show as This monitor.
const SCREENS: &[(&str, Label)] = &[
    ("this", client::player_screens_this),
    ("all", client::player_screens_all),
];
/// Presentation intent: `(stored value, display label)` — the `present_priority` key the
/// Apple and Android clients share, so one preset means the same thing everywhere.
const PRESENT_PRIORITIES: &[(&str, Label)] = &[
    ("latency", client::player_present_priority_latency),
    ("smooth", client::player_present_priority_smooth),
];
/// Smoothness buffer depths in frames, as stored. `0` = Automatic, which resolves to 2
/// (`PresentPriority::resolve`). No millisecond hints — the cost is one refresh per frame, and
/// the refresh isn't known here when the mode is Native.
const SMOOTH_BUFFERS: [u8; 4] = [0, 1, 2, 3];

/// The label of one [`SMOOTH_BUFFERS`] depth.
fn smooth_buffer_label(frames: u8) -> String {
    if frames == 0 {
        client::player_smooth_buffer_automatic().to_string()
    } else {
        client::player_smooth_buffer_frames(u64::from(frames))
    }
}
/// Host compositor presets: `(stored value, display label)`. Advisory — the host falls back to
/// auto-detect when the choice is unavailable. Only meaningful against a Linux host.
const COMPOSITORS: &[(&str, Label)] = &[
    ("auto", client::player_compositor_auto),
    ("kwin", || "KWin (KDE Plasma)"),
    ("mutter", || "Mutter (GNOME)"),
    ("hyprland", || "Hyprland"),
    ("niri", || "niri"),
    ("umbriel", || "Umbriel"),
    ("wlroots", || "wlroots (Sway, River)"),
    ("gamescope", || "gamescope"),
];

/// Display: resolution, quality, decoding, presentation, host output.
pub(super) fn display_section(cx: &Cx) -> Vec<View> {
    let Cx {
        ctx,
        scope,
        rev,
        set_rev,
        set_status,
        ref s,
        preset_mode,
        ..
    } = *cx;
    // The Aspect combo picks a family and lands on its size nearest the current height. The
    // Resolution combo is the D1 tri-state — Native, Match window (a virtual index 1, stored
    // as the `match_window` flag) — then that family's sizes.
    let family = aspect_of(s.width, s.height).unwrap_or(0);
    let aspect_combo = setting_combo(
        cx,
        scope,
        "resolution",
        ASPECTS.iter().map(|a| a.label.to_string()).collect(),
        family,
        |s, i| {
            s.match_window = false;
            (s.width, s.height) = nearest(i, s.height);
        },
    );
    // Native, Match window, the family's sizes, then Custom…, which shows Width and Height.
    // A size no family lists is Custom whatever the flag says.
    let sizes = ASPECTS[family].sizes;
    let custom_i = sizes.len() + 2;
    let custom =
        !s.match_window && s.width != 0 && (cx.custom_res || !sizes.contains(&(s.width, s.height)));
    let (res_names, res_i) = {
        let names: Vec<String> = [
            client::player_resolution_native().to_string(),
            client::player_resolution_match_window().to_string(),
        ]
        .into_iter()
        .chain(sizes.iter().map(|&(w, h)| format!("{w} \u{00D7} {h}")))
        .chain([client::player_resolution_custom().to_string()])
        .collect();
        let i = if s.match_window {
            1
        } else if custom {
            custom_i
        } else {
            sizes
                .iter()
                .position(|&(w, h)| w == s.width && h == s.height)
                .map_or(0, |i| i + 2)
        };
        (names, i)
    };
    let res_combo = {
        let set_custom = cx.set_custom_res.clone();
        setting_combo(cx, scope, "resolution", res_names, res_i, move |s, i| {
            set_custom.call(i == custom_i);
            s.match_window = i == 1;
            (s.width, s.height) = match i {
                0 | 1 => (0, 0),
                // Custom starts from the size shown, or 1080p from Native.
                i if i == custom_i && s.width == 0 => (1920, 1080),
                i if i == custom_i => (s.width, s.height),
                i => sizes[i - 2],
            };
        })
    };
    // Each box writes its side through the shared rule, keeping the other side as stored.
    let size_box = |value: u32, min: u32, width: bool| {
        let (ctx, scope) = (ctx.clone(), scope.to_string());
        let (set_rev, set_status) = (set_rev.clone(), set_status.clone());
        NumberBox::new()
            .value(f64::from(value))
            .minimum(f64::from(min))
            .maximum(8192.0)
            .on_value_changed(move |v: Option<f64>| {
                // A cleared box is no size: the stored one stands.
                let Some(v) = v else { return };
                commit(
                    &ctx,
                    &scope,
                    "resolution",
                    (rev, &set_rev),
                    &set_status,
                    |s| {
                        let typed = v.clamp(0.0, 8192.0) as u32;
                        let (w, h) = if width {
                            (typed, s.height)
                        } else {
                            (s.width, typed)
                        };
                        (s.width, s.height) = punktfunk_core::resolutions::custom(w, h, &s.codec);
                        s.match_window = false;
                    },
                );
            })
    };
    let res_control: View = if custom {
        vstack((
            View::from(res_combo),
            hstack((
                size_box(s.width, punktfunk_core::resolutions::MIN_WIDTH, true)
                    .header(client::player_resolution_width())
                    .width(120.0),
                text("\u{00D7}").vertical_alignment(VerticalAlignment::Bottom),
                size_box(s.height, punktfunk_core::resolutions::MIN_HEIGHT, false)
                    .header(client::player_resolution_height())
                    .width(120.0),
            ))
            .spacing(8.0),
        ))
        .spacing(8.0)
        .into()
    } else {
        res_combo.into()
    };
    let (hz_names, hz_i) = {
        let names: Vec<String> = REFRESH
            .iter()
            .map(|&r| {
                if r == 0 {
                    client::player_refresh_hz_native().into()
                } else {
                    common::common_hz(r)
                }
            })
            .collect();
        let i = REFRESH.iter().position(|&r| r == s.refresh_hz).unwrap_or(0);
        (names, i)
    };
    let hz_combo = setting_combo(cx, scope, "refresh_hz", hz_names, hz_i, |s, i| {
        s.refresh_hz = REFRESH[i];
    });
    // The stops, then Custom…, which shows a percent box. A scale that is no stop (typed
    // here or on another client) is Custom whatever the flag says.
    let scale_stop = punktfunk_core::render_scale::preset_index(s.render_scale);
    let custom_scale_i = RENDER_SCALES.len();
    let custom_scale = cx.custom_scale || scale_stop.is_none();
    let scale_names: Vec<String> = RENDER_SCALES
        .iter()
        .map(|&x| render_scale_label(x))
        .chain([client::player_render_scale_custom().to_string()])
        .collect();
    let scale_i = match scale_stop {
        Some(i) if !custom_scale => i,
        _ => custom_scale_i,
    };
    let scale_combo = {
        let set_custom = cx.set_custom_scale.clone();
        setting_combo(
            cx,
            scope,
            "render_scale",
            scale_names,
            scale_i,
            move |s, i| {
                set_custom.call(i == custom_scale_i);
                // Custom keeps the scale shown, for the box to start from.
                if let Some(&stop) = RENDER_SCALES.get(i) {
                    s.render_scale = stop;
                }
            },
        )
    };
    let scale_control: View = if custom_scale {
        let (ctx, scope) = (ctx.clone(), scope.to_string());
        let (set_rev, set_status) = (set_rev.clone(), set_status.clone());
        let percent = punktfunk_core::render_scale::percent(s.render_scale);
        vstack((
            View::from(scale_combo),
            NumberBox::new()
                .value(f64::from(percent))
                .minimum(50.0)
                .maximum(400.0)
                .header(client::player_render_scale_percent())
                .width(120.0)
                .on_value_changed(move |v: Option<f64>| {
                    // A cleared box is no scale, not Native.
                    let Some(v) = v.filter(|v| *v >= 1.0) else {
                        return;
                    };
                    commit(
                        &ctx,
                        &scope,
                        "render_scale",
                        (rev, &set_rev),
                        &set_status,
                        |s| {
                            let typed = v.clamp(1.0, 400.0) as u32;
                            s.render_scale = punktfunk_core::render_scale::from_percent(typed);
                        },
                    );
                }),
        ))
        .spacing(8.0)
        .into()
    } else {
        scale_combo.into()
    };
    let (comp_names, comp_i) = presets(&labeled(COMPOSITORS), |v| *v == s.compositor);
    let comp_combo = setting_combo(cx, scope, "compositor", comp_names, comp_i, |s, i| {
        s.compositor = COMPOSITORS[i].0.to_string();
    });
    // Migrated for the LOOKUP only (the store is left alone): a pre-M10 settings file
    // holds `vulkan`/`d3d11va`, which match no preset — the combo would show Automatic and
    // a save would silently rewrite the user's hardware preference to `auto`.
    let stored_decoder = pf_client_core::video::migrate_decoder_pref(&s.decoder);
    let (dec_names, dec_i) = presets(&labeled(DECODERS), |v| *v == stored_decoder);
    let decoder_combo = setting_combo(cx, scope, "decoder", dec_names, dec_i, |s, i| {
        s.decoder = DECODERS[i].0.to_string();
    });
    // GPU picker, only on a multi-GPU box (hybrid laptop, eGPU): which adapter decodes + presents.
    // Stored as the adapter description; empty = automatic (the window's monitor's adapter).
    let gpus = ctx.probes.lock().unwrap().gpus.clone();
    let gpu_combo = (gpus.len() > 1).then(|| {
        let mut names = vec![desktop::desktop_adapter_auto().to_string()];
        names.extend(gpus.iter().cloned());
        let current = gpus
            .iter()
            .position(|n| *n == s.adapter)
            .map_or(0, |i| i + 1);
        let gpus = gpus.clone();
        setting_combo(cx, scope, "adapter", names, current, move |s, i| {
            s.adapter = if i == 0 {
                String::new()
            } else {
                gpus[i - 1].clone()
            };
        })
    });
    let (codec_names, codec_i) = presets(&labeled(CODECS), |v| *v == s.codec);
    let codec_combo = setting_combo(cx, scope, "codec", codec_names, codec_i, |s, i| {
        s.codec = CODECS[i].0.to_string();
    });
    // Free-form Mb/s (0 = host default) instead of presets, so a speed-test recommendation
    // round-trips exactly. Through `commit` like every other row: writing `ctx.settings`
    // directly here would edit the GLOBAL defaults from inside a preset scope (and record
    // no override, so the row could never say "Overridden here").
    let bitrate_box = {
        let (ctx, scope) = (ctx.clone(), scope.to_string());
        let (set_rev, set_status) = (set_rev.clone(), set_status.clone());
        NumberBox::new()
            .value(f64::from(s.bitrate_kbps) / 1000.0)
            .minimum(0.0)
            .maximum(3000.0)
            .on_value_changed(move |v: Option<f64>| {
                // A cleared box is no bitrate: the stored one stands.
                let Some(v) = v else { return };
                commit(
                    &ctx,
                    &scope,
                    "bitrate_kbps",
                    (rev, &set_rev),
                    &set_status,
                    |s| {
                        s.bitrate_kbps = (v.clamp(0.0, 3000.0) * 1000.0) as u32;
                    },
                );
            })
    };
    // PyroWave sets its own rate: its quality stands where Bitrate stood, as the rate it needs
    // at the picture the host encodes, never as bits per pixel. The stored bitrate stays for the
    // other codecs.
    let pyrowave = s.codec == "pyrowave";
    let (quality_caption, quality_warning) = {
        let probes = ctx.probes.lock().unwrap();
        let native = probes.native.unwrap_or(punktfunk_core::Mode {
            width: 1920,
            height: 1080,
            refresh_hz: 60,
        });
        s.pyrowave_quality_lines(native, probes.link)
    };
    let quality_control = {
        use punktfunk_core::pyrowave::{BPP_FLOOR, BPP_MAX};
        let (ctx, scope) = (ctx.clone(), scope.to_string());
        let (set_rev, set_status) = (set_rev.clone(), set_status.clone());
        let slider = Slider::new()
            .value(s.pyrowave_bpp)
            .minimum(BPP_FLOOR)
            .maximum(BPP_MAX)
            .step_frequency(0.1)
            .on_value_changed(move |v: f64| {
                commit(
                    &ctx,
                    &scope,
                    "pyrowave_bpp",
                    (rev, &set_rev),
                    &set_status,
                    |s| {
                        s.pyrowave_bpp = (v * 10.0).round() / 10.0;
                    },
                );
            });
        // Always mounted, empty while the link carries the rate.
        let warning = text(quality_warning.as_deref().unwrap_or(""))
            .font_size(12.0)
            .foreground(cx.pal.get(Token::SystemCaution))
            .text_wrapping(TextWrapping::Wrap)
            .max_width(420.0)
            .horizontal_alignment(HorizontalAlignment::Left);
        vstack((View::from(slider), View::from(warning))).spacing(4.0)
    };
    let hdr_toggle = setting_toggle(cx, scope, "hdr_enabled", s.hdr_enabled, |s, on| {
        s.hdr_enabled = on
    });
    let ten_bit_sdr_toggle = setting_toggle(cx, scope, "ten_bit_sdr", s.ten_bit_sdr, |s, on| {
        s.ten_bit_sdr = on
    });
    let chroma_toggle = setting_toggle(cx, scope, "enable_444", s.enable_444, |s, on| {
        s.enable_444 = on
    });
    // Presentation intent (design/desktop-presentation-rebuild.md). The buffer row is
    // rendered only under Smoothness — `commit` bumps the revision, so flipping the
    // intent re-renders the section and the row appears/disappears with it.
    let (fit_names, fit_i) = presets(&labeled(VIDEO_FITS), |v| *v == s.video_fit);
    let fit_combo = setting_combo(cx, scope, "video_fit", fit_names, fit_i, |s, i| {
        s.video_fit = VIDEO_FITS[i].0.to_string();
    });
    let (screens_names, screens_i) = presets(&labeled(SCREENS), |v| *v == s.screens);
    let screens_combo = setting_combo(cx, scope, "screens", screens_names, screens_i, |s, i| {
        s.screens = SCREENS[i].0.to_string();
    });
    let (present_names, present_i) =
        presets(&labeled(PRESENT_PRIORITIES), |v| *v == s.present_priority);
    let present_combo = setting_combo(
        cx,
        scope,
        "present_priority",
        present_names,
        present_i,
        |s, i| s.present_priority = PRESENT_PRIORITIES[i].0.to_string(),
    );
    let smoothing = s.present_priority == "smooth";
    let buffer_labels: Vec<(u8, String)> = SMOOTH_BUFFERS
        .iter()
        .map(|&n| (n, smooth_buffer_label(n)))
        .collect();
    let (buffer_names, buffer_i) = presets(&buffer_labels, |v| *v == s.smooth_buffer);
    let buffer_combo = setting_combo(
        cx,
        scope,
        "smooth_buffer",
        buffer_names,
        buffer_i,
        |s, i| s.smooth_buffer = SMOOTH_BUFFERS[i],
    );
    let vsync_toggle = setting_toggle(cx, scope, "vsync", s.vsync, |s, on| s.vsync = on);
    let vrr_toggle = setting_toggle(cx, scope, "allow_vrr", s.allow_vrr, |s, on| {
        s.allow_vrr = on
    });

    let mut out = group(
        cx.pal,
        Some(client::prefs_group_resolution()),
        vec![
            described_labeled(
                cx.pal,
                client::player_resolution_aspect(),
                aspect_combo,
                client::player_resolution_aspect_hint(),
            ),
            described_overridable(
                cx,
                &rows::RESOLUTION,
                res_control,
                client::player_resolution_hint(),
            ),
            described_overridable(
                cx,
                &rows::REFRESH,
                hz_combo,
                client::player_refresh_hz_hint(),
            ),
        ],
        None,
    );
    let mut picture = Vec::new();
    if pyrowave {
        // Automatic or Fixed only: the quality row stands in for the rate.
        use pf_client_core::settings::PYROWAVE_FIXED_KBPS;
        let auto = setting_toggle(cx, scope, "bitrate_kbps", s.bitrate_kbps == 0, |s, on| {
            s.bitrate_kbps = if on { 0 } else { PYROWAVE_FIXED_KBPS };
        })
        .on_content(client::player_bitrate_kbps_automatic())
        .off_content(client::player_bitrate_kbps_fixed());
        picture.push(described_overridable(
            cx,
            &rows::BITRATE,
            auto,
            client::player_bitrate_kbps_pyrowave_hint(),
        ));
    } else {
        picture.push(described_overridable(
            cx,
            &rows::BITRATE,
            bitrate_box,
            desktop::desktop_bitrate_kbps_hint(),
        ));
    }
    picture.extend([
        described_overridable(
            cx,
            &rows::VIDEO_FIT,
            fit_combo,
            client::player_video_fit_hint(),
        ),
        described_overridable(
            cx,
            &rows::SCREENS,
            screens_combo,
            client::player_screens_hint(),
        ),
        described_overridable(
            cx,
            &rows::HDR,
            hdr_toggle,
            client::player_hdr_enabled_hint(),
        ),
        described_overridable(
            cx,
            &rows::PRESENT,
            present_combo,
            client::player_present_priority_hint(),
        ),
        described_overridable(cx, &rows::CODEC, codec_combo, client::player_codec_hint()),
    ]);
    if pyrowave {
        picture.push(described_overridable(
            cx,
            &rows::PYROWAVE_QUALITY,
            quality_control,
            &quality_caption,
        ));
    }
    out.extend(group(
        cx.pal,
        Some(client::prefs_group_picture()),
        picture,
        // The one form-level note, exactly as on Apple.
        Some(client::prefs_applies_next_session()),
    ));

    let d = Settings::default();
    let mut advanced = Vec::new();
    if smoothing {
        advanced.push(described_overridable(
            cx,
            &rows::SMOOTH_BUFFER,
            buffer_combo,
            client::player_smooth_buffer_hint(),
        ));
    }
    advanced.extend([
        described_overridable(
            cx,
            &rows::RENDER_SCALE,
            scale_control,
            client::player_render_scale_hint(),
        ),
        // First sentence shared with the GTK client (its chroma_row); the constraint
        // sentence names the real gate (host: PyroWave || NVENC).
        described_overridable(
            cx,
            &rows::CHROMA,
            chroma_toggle,
            client::player_enable_444_hint(),
        ),
        described_overridable(
            cx,
            &rows::TEN_BIT_SDR,
            ten_bit_sdr_toggle,
            client::player_ten_bit_sdr_hint(),
        ),
        described_overridable(cx, &rows::VSYNC, vsync_toggle, client::player_vsync_hint()),
        described_overridable(cx, &rows::VRR, vrr_toggle, client::player_allow_vrr_hint()),
        described_overridable(
            cx,
            &rows::COMPOSITOR,
            comp_combo,
            client::player_compositor_hint(),
        ),
    ]);
    // Decoder, GPU and the screen saver are facts about THIS device — never per preset.
    if !preset_mode {
        advanced.push(described_labeled(
            cx.pal,
            rows::KEEP_AWAKE.label(),
            setting_toggle(cx, scope, "keep_awake", s.keep_awake, |s, on| {
                s.keep_awake = on
            }),
            client::player_keep_awake_hint(),
        ));
        advanced.push(described_labeled(
            cx.pal,
            rows::DECODER.label(),
            decoder_combo,
            client::player_decoder_hint(),
        ));
        if let Some(c) = gpu_combo {
            advanced.push(described_labeled(
                cx.pal,
                rows::ADAPTER.label(),
                c,
                desktop::desktop_adapter_hint(),
            ));
        }
    }
    let changed = [
        smoothing && s.smooth_buffer != d.smooth_buffer,
        s.render_scale != d.render_scale,
        s.enable_444 != d.enable_444,
        s.ten_bit_sdr != d.ten_bit_sdr,
        s.vsync != d.vsync,
        s.allow_vrr != d.allow_vrr,
        s.compositor != d.compositor,
        stored_decoder != d.decoder,
        !s.adapter.is_empty(),
        s.keep_awake != d.keep_awake,
    ];
    let overridden = [
        "smooth_buffer",
        "render_scale",
        "enable_444",
        "ten_bit_sdr",
        "vsync",
        "allow_vrr",
        "compositor",
    ]
    .into_iter()
    .any(|f| cx.overrides(f));
    out.extend(advanced_group(
        cx,
        advanced,
        changed.into_iter().filter(|c| *c).count(),
        overridden,
    ));
    out
}
