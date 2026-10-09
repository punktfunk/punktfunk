//! Display: resolution, picture, decoding, presentation and host output.

use super::{
    advanced_group, commit, described_labeled, described_overridable, group, presets,
    setting_combo, setting_toggle, Cx,
};
use crate::trust::Settings;
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
        "Native".to_string()
    } else if scale > 1.0 {
        format!("{scale}\u{00D7} (supersample)")
    } else {
        format!("{scale}\u{00D7}")
    }
}
/// Decode backend presets: `(stored value, display label)`.
// A stored legacy value that matches no preset (the D3D11VA-era "hardware", and since M10
// the bare "vulkan"/"d3d11va" that named libavcodec's rungs) shows as Automatic — which is
// how the session's ladder reads "hardware", and near enough for the other two, which
// `pf_client_core::video::migrate_decoder_pref` maps onto the entries below anyway.
const DECODERS: &[(&str, &str)] = &[
    ("auto", "Automatic (GPU, fall back to CPU)"),
    ("native-vulkan", "Hardware (Vulkan Video)"),
    ("native-d3d11va", "Hardware (Direct3D 11 / DXVA)"),
    ("software", "Software (CPU)"),
];
/// Preferred-codec presets: `(stored value, display label)`. Soft — the host falls back if it
/// can't encode the chosen codec.
const CODECS: &[(&str, &str)] = &[
    ("auto", "Automatic"),
    ("hevc", "HEVC (H.265)"),
    ("h264", "H.264 (AVC)"),
    ("av1", "AV1"),
    // Preference-only by design: `resolve_codec` never auto-picks PyroWave, and asking for
    // it on a host or device that can't do it simply falls back down the ladder to HEVC.
    ("pyrowave", "PyroWave (wired LAN)"),
];
/// `video_fit`: `(stored value, display label)`. Unknown values show as Fit.
const VIDEO_FITS: &[(&str, &str)] = &[
    ("fit", "Fit"),
    ("crop", "Crop to fill"),
    ("stretch", "Stretch to fill"),
];
/// Presentation intent: `(stored value, display label)` — the `present_priority` key the
/// Apple and Android clients share, so one preset means the same thing everywhere.
const PRESENT_PRIORITIES: &[(&str, &str)] =
    &[("latency", "Lowest latency"), ("smooth", "Smoothness")];
/// Smoothness buffer depth in frames: `(stored value, display label)`. `0` = Automatic,
/// which resolves to 2 (`PresentPriority::resolve`). No millisecond hints — the cost is
/// one refresh per frame, and the refresh isn't known here when the mode is Native.
const SMOOTH_BUFFERS: &[(u8, &str)] = &[
    (0, "Automatic"),
    (1, "1 frame"),
    (2, "2 frames"),
    (3, "3 frames"),
];
/// Host compositor presets: `(stored value, display label)`. Advisory — the host falls back to
/// auto-detect when the choice is unavailable. Only meaningful against a Linux host.
const COMPOSITORS: &[(&str, &str)] = &[
    ("auto", "Automatic"),
    ("kwin", "KWin"),
    ("mutter", "Mutter (GNOME)"),
    ("hyprland", "Hyprland"),
    ("wlroots", "wlroots (Sway/River)"),
    ("gamescope", "gamescope"),
];

/// Display: resolution, quality, decoding, presentation, host output.
pub(super) fn display_section(cx: &Cx) -> Vec<Element> {
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
        let names: Vec<String> = ["Native display".to_string(), "Match window".to_string()]
            .into_iter()
            .chain(sizes.iter().map(|&(w, h)| format!("{w} \u{00D7} {h}")))
            .chain(["Custom\u{2026}".to_string()])
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
        NumberBox::new(f64::from(value))
            .range(f64::from(min), 8192.0)
            .on_value_changed(move |v: f64| {
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
    let res_control: Element = if custom {
        vstack((
            Element::from(res_combo),
            hstack((
                size_box(s.width, punktfunk_core::resolutions::MIN_WIDTH, true)
                    .header("Width")
                    .width(120.0),
                text_block("\u{00D7}").vertical_alignment(VerticalAlignment::Bottom),
                size_box(s.height, punktfunk_core::resolutions::MIN_HEIGHT, false)
                    .header("Height")
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
                    "Native".into()
                } else {
                    format!("{r} Hz")
                }
            })
            .collect();
        let i = REFRESH.iter().position(|&r| r == s.refresh_hz).unwrap_or(0);
        (names, i)
    };
    let hz_combo = setting_combo(cx, scope, "refresh_hz", hz_names, hz_i, |s, i| {
        s.refresh_hz = REFRESH[i];
    });
    let (scale_names, scale_i) = {
        let names: Vec<String> = RENDER_SCALES
            .iter()
            .map(|&x| render_scale_label(x))
            .collect();
        let i = RENDER_SCALES
            .iter()
            .position(|&x| (x - s.render_scale).abs() < 1e-6)
            .unwrap_or_else(|| RENDER_SCALES.iter().position(|&x| x == 1.0).unwrap());
        (names, i)
    };
    let scale_combo = setting_combo(cx, scope, "render_scale", scale_names, scale_i, |s, i| {
        s.render_scale = RENDER_SCALES[i];
    });
    let (comp_names, comp_i) = presets(COMPOSITORS, |v| *v == s.compositor);
    let comp_combo = setting_combo(cx, scope, "compositor", comp_names, comp_i, |s, i| {
        s.compositor = COMPOSITORS[i].0.to_string();
    });
    // Migrated for the LOOKUP only (the store is left alone): a pre-M10 settings file
    // holds `vulkan`/`d3d11va`, which match no preset — the combo would show Automatic and
    // a save would silently rewrite the user's hardware preference to `auto`.
    let stored_decoder = pf_client_core::video::migrate_decoder_pref(&s.decoder);
    let (dec_names, dec_i) = presets(DECODERS, |v| *v == stored_decoder);
    let decoder_combo = setting_combo(cx, scope, "decoder", dec_names, dec_i, |s, i| {
        s.decoder = DECODERS[i].0.to_string();
    });
    // GPU picker, only on a multi-GPU box (hybrid laptop, eGPU): which adapter decodes + presents.
    // Stored as the adapter description; empty = automatic (the window's monitor's adapter).
    let gpus = ctx.probes.lock().unwrap().gpus.clone();
    let gpu_combo = (gpus.len() > 1).then(|| {
        let mut names = vec!["Automatic (the display's GPU)".to_string()];
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
    let (codec_names, codec_i) = presets(CODECS, |v| *v == s.codec);
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
        NumberBox::new(f64::from(s.bitrate_kbps) / 1000.0)
            .range(0.0, 3000.0)
            .on_value_changed(move |v: f64| {
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
    // at the mode a connect would ask, never as bits per pixel. The stored bitrate stays for the
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
        let slider = Slider::new(s.pyrowave_bpp)
            .range(BPP_FLOOR, BPP_MAX)
            .step(0.1)
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
        let warning = text_block(quality_warning.as_deref().unwrap_or(""))
            .font_size(12.0)
            .foreground(ThemeRef::SystemCaution)
            .wrap()
            .max_width(420.0)
            .horizontal_alignment(HorizontalAlignment::Left);
        vstack((Element::from(slider), Element::from(warning))).spacing(4.0)
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
    let (fit_names, fit_i) = presets(VIDEO_FITS, |v| *v == s.video_fit);
    let fit_combo = setting_combo(cx, scope, "video_fit", fit_names, fit_i, |s, i| {
        s.video_fit = VIDEO_FITS[i].0.to_string();
    });
    let (present_names, present_i) = presets(PRESENT_PRIORITIES, |v| *v == s.present_priority);
    let present_combo = setting_combo(
        cx,
        scope,
        "present_priority",
        present_names,
        present_i,
        |s, i| s.present_priority = PRESENT_PRIORITIES[i].0.to_string(),
    );
    let smoothing = s.present_priority == "smooth";
    let (buffer_names, buffer_i) = presets(SMOOTH_BUFFERS, |v| *v == s.smooth_buffer);
    let buffer_combo = setting_combo(
        cx,
        scope,
        "smooth_buffer",
        buffer_names,
        buffer_i,
        |s, i| s.smooth_buffer = SMOOTH_BUFFERS[i].0,
    );
    let vsync_toggle = setting_toggle(cx, scope, "vsync", s.vsync, |s, on| s.vsync = on);
    let vrr_toggle = setting_toggle(cx, scope, "allow_vrr", s.allow_vrr, |s, on| {
        s.allow_vrr = on
    });

    let mut out = group(
        Some("Resolution"),
        vec![
            described_labeled(
                "Aspect ratio",
                aspect_combo,
                "Which shapes the Resolution list offers. Picking one moves to its \
                 size nearest the current height.",
            ),
            described_overridable(
                cx,
                "resolution",
                "Resolution",
                res_control,
                "The host drives a real virtual output at exactly this size \u{2014} true \
                 pixels, no scaling. \u{201C}Native display\u{201D} follows the monitor this \
                 window is on; \u{201C}Match window\u{201D} keeps the picture pixel-exact \
                 (1:1) through every resize.",
            ),
            described_overridable(
                cx,
                "refresh_hz",
                "Refresh rate",
                hz_combo,
                "\u{201C}Native\u{201D} resolves to this display\u{2019}s refresh rate at \
                 connect.",
            ),
        ],
        None,
    );
    let mut picture = Vec::new();
    if !pyrowave {
        picture.push(described_overridable(
            cx,
            "bitrate_kbps",
            "Bitrate (Mb/s, 0 = automatic)",
            bitrate_box,
            "0 lets the host decide (its default, clamped to what it supports). A host \
             card\u{2019}s context menu has a network speed test.",
        ));
    }
    picture.extend([
        described_overridable(
            cx,
            "video_fit",
            "Picture fit",
            fit_combo,
            "When the stream's shape differs from the window. Fit shows the whole \
             picture with black bars, Crop to fill cuts the edges off, Stretch to \
             fill distorts it.",
        ),
        described_overridable(
            cx,
            "hdr_enabled",
            "10-bit HDR",
            hdr_toggle,
            "HDR10, when the host has HDR content and this display supports it. \
             With H.264 the stream stays SDR.",
        ),
        described_overridable(
            cx,
            "present_priority",
            "Prioritize",
            present_combo,
            "Lowest latency shows each frame the moment the display can take \
             it \u{2014} a network hiccup becomes an occasional repeated or \
             skipped frame. Smoothness buffers a little to even those out.",
        ),
    ]);
    out.extend(group(
        Some("Picture"),
        picture,
        // The one form-level note, exactly as on Apple.
        Some("Display changes apply from the next session."),
    ));

    let d = Settings::default();
    let mut advanced = Vec::new();
    if smoothing {
        advanced.push(described_overridable(
            cx,
            "smooth_buffer",
            "Smoothness buffer",
            buffer_combo,
            "Frames held back before showing. Each one absorbs about a refresh of \
             network hiccup and adds a refresh of delay. Automatic holds two.",
        ));
    }
    advanced.extend([
        described_overridable(
            cx,
            "render_scale",
            "Render scale",
            scale_combo,
            "Above native supersamples for sharpness; below renders lighter on the \
             host and the link. This device resamples the result to the window.",
        ),
        described_overridable(
            cx,
            "codec",
            "Video codec",
            codec_combo,
            "A preference \u{2014} the host falls back if it can\u{2019}t encode it. \
             PyroWave is the low-latency wavelet codec for a WIRED link: it trades \
             bitrate (hundreds of Mb/s) for near-zero decode time, so it wants \
             gigabit Ethernet.",
        ),
    ]);
    if pyrowave {
        advanced.push(described_overridable(
            cx,
            "pyrowave_bpp",
            "PyroWave quality",
            quality_control,
            &quality_caption,
        ));
    }
    advanced.extend([
        // First sentence shared with the GTK client (its chroma_row); the constraint
        // sentence names the real gate (host: PyroWave || NVENC).
        described_overridable(
            cx,
            "enable_444",
            "Full chroma (4:4:4)",
            chroma_toggle,
            "Full-colour video: crisp small text and thin lines, at more bandwidth. \
             Requires an NVIDIA host (NVENC) or the PyroWave codec \u{2014} other \
             encoders stream 4:2:0.",
        ),
        described_overridable(
            cx,
            "ten_bit_sdr",
            "10-bit SDR",
            ten_bit_sdr_toggle,
            "Smoother gradients without HDR \u{2014} the picture is encoded at 10-bit \
             precision. Needs an NVIDIA host; HDR takes over when it engages.",
        ),
        described_overridable(
            cx,
            "vsync",
            "V-Sync",
            vsync_toggle,
            "Tear-free. Turning it off removes the wait for the screen\u{2019}s refresh \
             \u{2014} the lowest possible delay, at the cost of visible tearing. Not \
             every driver offers it; the stats overlay names the mode actually in use.",
        ),
        described_overridable(
            cx,
            "allow_vrr",
            "Follow variable refresh",
            vrr_toggle,
            "On a VRR/FreeSync/G-Sync screen, let the panel refresh in step with the \
             stream instead of on a fixed cadence. Applies to fullscreen sessions; \
             harmless on a fixed-refresh screen.",
        ),
        described_overridable(
            cx,
            "compositor",
            "Host compositor",
            comp_combo,
            "The backend the host uses for its virtual output (Linux hosts only). A \
             specific choice falls back to auto-detection when that backend \
             isn\u{2019}t available.",
        ),
    ]);
    // Decoder and GPU are facts about THIS device's hardware — never per preset.
    if !preset_mode {
        advanced.push(described_labeled(
            "Video decoder",
            decoder_combo,
            "Automatic picks the hardware path this GPU does best \u{2014} Direct3D 11 on \
             Intel, Vulkan Video on NVIDIA and AMD \u{2014} and falls back to the CPU. \
             Change it only when debugging.",
        ));
        if let Some(c) = gpu_combo {
            advanced.push(described_labeled(
                "GPU",
                c,
                "Which adapter decodes and presents the stream. Automatic uses the GPU \
                 driving this window\u{2019}s display.",
            ));
        }
    }
    let changed = [
        smoothing && s.smooth_buffer != d.smooth_buffer,
        s.render_scale != d.render_scale,
        s.codec != d.codec,
        pyrowave && s.pyrowave_bpp_x100() != d.pyrowave_bpp_x100(),
        s.enable_444 != d.enable_444,
        s.ten_bit_sdr != d.ten_bit_sdr,
        s.vsync != d.vsync,
        s.allow_vrr != d.allow_vrr,
        s.compositor != d.compositor,
        stored_decoder != d.decoder,
        !s.adapter.is_empty(),
    ];
    let overridden = [
        "smooth_buffer",
        "render_scale",
        "codec",
        "enable_444",
        "ten_bit_sdr",
        "vsync",
        "allow_vrr",
        "compositor",
    ]
    .into_iter()
    .any(|f| cx.overrides(f))
        || (pyrowave && cx.overrides("pyrowave_bpp"));
    out.extend(advanced_group(
        cx,
        advanced,
        changed.into_iter().filter(|c| *c).count(),
        overridden,
    ));
    out
}
