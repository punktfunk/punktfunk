//! Display: the stream's size and rate, the picture, and the advanced encode and present rows.

use super::{Build, PageB};
use crate::settings::choice::{set_row_subtitle, ChoiceRow};
use crate::settings::field::Field;
use crate::settings::spec::{self, Page};
use crate::settings::tables::*;
use crate::trust::Settings;
use adw::prelude::*;
use pf_l10n::{client, common, desktop};
use std::cell::Cell;
use std::rc::Rc;

pub fn display(b: &mut Build) {
    let mut p = b.page(Page::Display, "video-display-symbolic");
    let size = p.group(client::prefs_group_resolution(), "");
    let res = resolution(b);
    b.put(&mut p, Some(&size), res);
    let names: Vec<String> = REFRESH
        .iter()
        .map(|&r| match r {
            0 => client::player_refresh_hz_native().to_string(),
            r => common::common_hz(r),
        })
        .collect();
    let row = b.choice(
        &spec::REFRESH,
        &names.iter().map(String::as_str).collect::<Vec<_>>(),
    );
    b.put(
        &mut p,
        Some(&size),
        Field::choice(&spec::REFRESH, &row, index::refresh, |s, i| {
            s.refresh_hz = *at(REFRESH, i)
        }),
    );

    // The one form-level note, not repeated on every row.
    let picture = p.group(
        client::prefs_group_picture(),
        client::prefs_applies_next_session(),
    );
    let (bitrate_field, show_pyrowave_bitrate) = bitrate_row(b);
    b.put(&mut p, Some(&picture), bitrate_field);
    let fit = b.choice(&spec::VIDEO_FIT, &texts(VIDEO_FIT_LABELS));
    caption_follows(&fit, VIDEO_FIT_CAPTIONS);
    b.put(
        &mut p,
        Some(&picture),
        Field::choice(&spec::VIDEO_FIT, &fit, index::video_fit, |s, i| {
            s.video_fit = at(VIDEO_FITS, i).to_string()
        }),
    );
    let screens = b.choice(&spec::SCREENS, &texts(SCREENS_LABELS));
    caption_follows(&screens, SCREENS_CAPTIONS);
    b.put(
        &mut p,
        Some(&picture),
        Field::choice(&spec::SCREENS, &screens, index::screens, |s, i| {
            s.screens = at(SCREENS, i).to_string()
        }),
    );
    let (field, _) = Field::switch(&spec::HDR, |s| s.hdr_enabled, |s, v| s.hdr_enabled = v);
    b.put(&mut p, Some(&picture), field);
    // The buffer only means something under Smoothness, so it hides the rest of the time.
    let present = b.choice(&spec::PRESENT, &texts(PRESENT_PRIORITY_LABELS));
    let buffer_names = smooth_buffer_labels();
    let buffer = b.choice(
        &spec::SMOOTH_BUFFER,
        &buffer_names.iter().map(String::as_str).collect::<Vec<_>>(),
    );
    caption_follows(&present, PRESENT_PRIORITY_CAPTIONS);
    let w = buffer.widget().clone();
    w.set_visible(false);
    present.connect_changed(move |i| w.set_visible(*at(PRESENT_PRIORITIES, i) == "smooth"));
    b.put(
        &mut p,
        Some(&picture),
        Field::choice(&spec::PRESENT, &present, index::present_priority, |s, i| {
            s.present_priority = at(PRESENT_PRIORITIES, i).to_string()
        }),
    );

    b.put(
        &mut p,
        None,
        Field::choice(
            &spec::SMOOTH_BUFFER,
            &buffer,
            index::smooth_buffer,
            |s, i| s.smooth_buffer = i.min(SMOOTH_BUFFER_STEPS - 1) as u8,
        ),
    );
    let scale = render_scale(b);
    b.put(&mut p, None, scale);
    let codec = b.choice(&spec::CODEC, &texts(CODEC_LABELS));
    let (quality_field, show_quality) = pyrowave_quality_row(b);
    {
        let w = codec.widget().clone();
        // PyroWave sets its own rate: its quality stands in for Bitrate's, Automatic stays.
        codec.connect_changed(move |i| {
            set_row_subtitle(&w, codec_caption(i));
            let pyrowave = *at(CODECS, i) == "pyrowave";
            show_pyrowave_bitrate(pyrowave);
            show_quality(pyrowave);
        });
    }
    b.put(
        &mut p,
        Some(&picture),
        Field::choice(&spec::CODEC, &codec, index::codec, |s, i| {
            s.codec = at(CODECS, i).to_string()
        }),
    );
    b.put(&mut p, Some(&picture), quality_field);
    type Switch = (
        &'static spec::Spec,
        fn(&Settings) -> bool,
        fn(&mut Settings, bool),
    );
    let switches: [Switch; 4] = [
        (&spec::CHROMA, |s| s.enable_444, |s, v| s.enable_444 = v),
        (
            &spec::TEN_BIT_SDR,
            |s| s.ten_bit_sdr,
            |s, v| s.ten_bit_sdr = v,
        ),
        (&spec::VSYNC, |s| s.vsync, |s, v| s.vsync = v),
        (&spec::VRR, |s| s.allow_vrr, |s, v| s.allow_vrr = v),
    ];
    for (spec, get, set) in switches {
        b.put(&mut p, None, Field::switch(spec, get, set).0);
    }
    let row = b.choice(
        &spec::COMPOSITOR,
        &[
            client::player_compositor_auto(),
            "KWin",
            "Mutter (GNOME)",
            "Hyprland",
            "niri",
            "Umbriel",
            "wlroots (Sway/River)",
            "gamescope",
        ],
    );
    b.put(
        &mut p,
        None,
        Field::choice(&spec::COMPOSITOR, &row, index::compositor, |s, i| {
            s.compositor = at(COMPOSITORS, i).to_string()
        }),
    );
    hardware(b, &mut p);
    b.finish(p);
}

/// Decoder, GPU and the screen's own saver: facts about this device, never a preset's.
fn hardware(b: &mut Build, p: &mut PageB) {
    let (awake, _) = Field::switch(&spec::KEEP_AWAKE, |s| s.keep_awake, |s, v| s.keep_awake = v);
    b.put(p, None, awake);
    let row = b.choice(
        &spec::DECODER,
        &[
            client::player_decoder_auto(),
            "Vulkan Video",
            "VAAPI",
            client::player_decoder_software(),
        ],
    );
    b.put(
        p,
        None,
        Field::choice(
            &spec::DECODER,
            &row,
            // A pre-M10 `vulkan`/`vaapi` is migrated for the lookup only.
            |s| {
                let stored = pf_client_core::video::migrate_decoder_pref(&s.decoder);
                DECODERS.iter().position(|&d| d == stored).unwrap_or(0) as u32
            },
            |s, i| s.decoder = at(DECODERS, i).to_string(),
        ),
    );
    // The adapter feeds the session's device pick. Hidden with nothing to pick; a saved one
    // that is gone (an unplugged eGPU) keeps an entry to move off.
    let saved = b.seed.adapter.clone();
    let mut names = vec![desktop::desktop_adapter_auto().to_string()];
    let mut keys = vec![String::new()];
    for a in &b.probes.adapters {
        names.push(a.clone());
        keys.push(a.clone());
    }
    if !saved.is_empty() && !keys.contains(&saved) {
        names.push(desktop::desktop_adapter_missing(&saved));
        keys.push(saved);
    }
    if keys.len() > 1 {
        let row = b.choice(
            &spec::ADAPTER,
            &names.iter().map(String::as_str).collect::<Vec<_>>(),
        );
        let keys = Rc::new(keys);
        let k = keys.clone();
        b.put(
            p,
            None,
            Field::choice(
                &spec::ADAPTER,
                &row,
                move |s| k.iter().position(|k| *k == s.adapter).unwrap_or(0) as u32,
                move |s, i| s.adapter = at(&keys, i).clone(),
            ),
        );
    }
}

/// Aspect, Resolution, and the typed Width and Height behind Custom: one setting, the D1
/// tri-state of Native, Match window and a size. Aspect only picks the sizes listed.
fn resolution(b: &Build) -> Field {
    let res = b.choice(
        &spec::RESOLUTION,
        &resolution_names(0)
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
    );
    let width = size_row(
        client::player_resolution_width(),
        punktfunk_core::resolutions::MIN_WIDTH,
    );
    let height = size_row(
        client::player_resolution_height(),
        punktfunk_core::resolutions::MIN_HEIGHT,
    );
    let aspect = ChoiceRow::new(
        &b.dialog,
        b.inline,
        client::player_resolution_aspect(),
        client::player_resolution_aspect_hint(),
        &ASPECTS.iter().map(|a| a.label).collect::<Vec<_>>(),
    );
    // The family the Resolution row lists; only the Aspect handler moves it.
    let shown = Rc::new(Cell::new(0usize));
    {
        let (w, shown) = (res.widget().clone(), shown.clone());
        let (width, height) = (width.clone(), height.clone());
        res.connect_changed(move |i| {
            set_row_subtitle(&w, resolution_caption(i));
            let custom = i == index::custom(shown.get());
            width.set_visible(custom);
            height.set_visible(custom);
        });
    }
    {
        let (res, shown, height) = (res.clone(), shown.clone(), height.clone());
        aspect.connect_changed(move |g| {
            // Re-list the family and land on its size nearest the one shown. Native and
            // Match window count as 1080 (`nearest`); a typed size counts as its height.
            let g = g as usize;
            let at = res.selected();
            let h = if at == index::custom(shown.get()) {
                height.value() as u32
            } else {
                (at as usize)
                    .checked_sub(2)
                    .and_then(|i| ASPECTS[shown.get()].sizes.get(i))
                    .map_or(0, |&(_, h)| h)
            };
            shown.set(g);
            res.set_options(&resolution_names(g));
            let target = nearest(g, h);
            let i = ASPECTS[g].sizes.iter().position(|&wh| wh == target);
            res.set_selected(i.map_or(0, |i| i as u32 + 2));
        });
    }
    let widgets = vec![
        aspect.widget().clone().upcast(),
        res.widget().clone().upcast(),
        width.clone().upcast(),
        height.clone().upcast(),
    ];
    let row: adw::ActionRow = res.widget().clone().downcast().expect("an action row");
    let seed = {
        let (res, aspect, width, height) =
            (res.clone(), aspect.clone(), width.clone(), height.clone());
        move |s: &crate::trust::Settings| {
            // The typed size starts from the stored one, or 1080p under Native and Match window.
            let (w, h) = if s.width == 0 {
                (1920, 1080)
            } else {
                (s.width, s.height)
            };
            width.set_value(f64::from(w));
            height.set_value(f64::from(h));
            aspect.set_selected(index::aspect(s));
            res.set_selected(index::resolution(s));
        }
    };
    let write = {
        let (res, width, height, shown) = (res.clone(), width.clone(), height.clone(), shown);
        move |s: &mut crate::trust::Settings| {
            let sizes = ASPECTS[shown.get()].sizes;
            let custom = sizes.len() + 2;
            let i = (res.selected() as usize).min(custom);
            s.match_window = i == 1;
            (s.width, s.height) = match i {
                0 | 1 => (0, 0),
                i if i == custom => punktfunk_core::resolutions::custom(
                    width.value() as u32,
                    height.value() as u32,
                    &s.codec,
                ),
                i => sizes[i - 2],
            };
        }
    };
    let watch = {
        let (res, width, height) = (res.clone(), width, height);
        move |f: Rc<dyn Fn()>| {
            let g = f.clone();
            res.connect_changed(move |_| g());
            for row in [&width, &height] {
                let g = f.clone();
                row.connect_value_notify(move |_| g());
            }
        }
    };
    Field::new(
        &spec::RESOLUTION,
        &row,
        widgets,
        seed,
        write,
        || false,
        watch,
    )
}

/// Render scale and the typed Percent behind Custom: the stops, then Custom…. A stored scale
/// that is no stop seeds Custom, with its percent in the row.
fn render_scale(b: &Build) -> Field {
    let names: Vec<String> = RENDER_SCALES
        .iter()
        .map(|&s| render_scale_label(s))
        .chain([client::player_render_scale_custom().to_string()])
        .collect();
    let choice = b.choice(
        &spec::RENDER_SCALE,
        &names.iter().map(String::as_str).collect::<Vec<_>>(),
    );
    let percent = adw::SpinRow::with_range(50.0, 400.0, 1.0);
    percent.set_title(client::player_render_scale_percent());
    percent.set_visible(false);
    {
        let percent = percent.clone();
        choice.connect_changed(move |i| percent.set_visible(i == index::CUSTOM_SCALE));
    }
    let widgets = vec![choice.widget().clone().upcast(), percent.clone().upcast()];
    let row: adw::ActionRow = choice.widget().clone().downcast().expect("an action row");
    let seed = {
        let (choice, percent) = (choice.clone(), percent.clone());
        move |s: &Settings| {
            let at = index::render_scale(s);
            percent.set_value(f64::from(punktfunk_core::render_scale::percent(
                s.render_scale,
            )));
            percent.set_visible(at == index::CUSTOM_SCALE);
            choice.set_selected(at);
        }
    };
    let write = {
        let (choice, percent) = (choice.clone(), percent.clone());
        move |s: &mut Settings| {
            s.render_scale = match RENDER_SCALES.get(choice.selected() as usize) {
                Some(&stop) => stop,
                None => punktfunk_core::render_scale::from_percent(percent.value() as u32),
            };
        }
    };
    let watch = {
        let (choice, percent) = (choice.clone(), percent);
        move |f: Rc<dyn Fn()>| {
            let g = f.clone();
            choice.connect_changed(move |_| g());
            percent.connect_value_notify(move |_| f());
        }
    };
    Field::new(
        &spec::RENDER_SCALE,
        &row,
        widgets,
        seed,
        write,
        || false,
        watch,
    )
}

/// A Width or Height row for a typed size, hidden until Resolution is on Custom.
fn size_row(title: &str, min: u32) -> adw::SpinRow {
    let row = adw::SpinRow::with_range(f64::from(min), 8192.0, 2.0);
    row.set_title(title);
    row.set_visible(false);
    row
}

/// 1 Mbit/s per step: the rungs that matter on a thin link are 3, 4, 6. Under PyroWave the
/// quality row stands in for the rate and Bitrate offers Automatic or Fixed, which only move
/// the spin between 0 and a fixed rate. The returned closure swaps the two rows.
fn bitrate_row(b: &Build) -> (Field, Rc<dyn Fn(bool)>) {
    use pf_client_core::settings::PYROWAVE_FIXED_KBPS;
    let row = adw::SpinRow::with_range(0.0, 3000.0, 1.0);
    row.set_title(spec::BITRATE.title());
    row.set_subtitle(&spec::BITRATE.caption());
    let auto = b.choice(
        &spec::BITRATE,
        &[
            client::player_bitrate_kbps_automatic(),
            client::player_bitrate_kbps_fixed(),
        ],
    );
    set_row_subtitle(auto.widget(), client::player_bitrate_kbps_pyrowave_hint());
    auto.widget().set_visible(false);
    {
        let r = row.clone();
        auto.connect_changed(move |i| {
            let fixed = i == 1;
            if fixed == (r.value() == 0.0) {
                r.set_value(if fixed {
                    f64::from(PYROWAVE_FIXED_KBPS) / 1000.0
                } else {
                    0.0
                });
            }
        });
        let a = auto.clone();
        row.connect_value_notify(move |r| {
            let fixed = u32::from(r.value() != 0.0);
            if a.selected() != fixed {
                a.set_selected(fixed);
            }
        });
    }
    let show: Rc<dyn Fn(bool)> = {
        let (row, auto) = (row.clone(), auto.widget().clone());
        Rc::new(move |pyrowave| {
            row.set_visible(!pyrowave);
            auto.set_visible(pyrowave);
        })
    };
    let r = row.clone();
    let field = Field::new(
        &spec::BITRATE,
        &row,
        vec![row.clone().upcast(), auto.widget().clone().upcast()],
        {
            let r = r.clone();
            move |s| r.set_value(f64::from(s.bitrate_kbps) / 1000.0)
        },
        {
            let r = r.clone();
            move |s| s.bitrate_kbps = (r.value() * 1000.0) as u32
        },
        || false,
        move |f| {
            r.connect_value_notify(move |_| f());
        },
    );
    (field, show)
}

/// PyroWave quality: a slider from 0.5 to 2 bits per pixel by tenths that never shows them.
/// Its caption is the rate the quality needs at the mode a connect would ask, re-read as any
/// row moves; a warning row follows while this device's link is short of it. The returned
/// closure shows both under PyroWave.
fn pyrowave_quality_row(b: &Build) -> (Field, Rc<dyn Fn(bool)>) {
    use punktfunk_core::pyrowave::{BPP_DEFAULT, BPP_FLOOR, BPP_MAX};
    let scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, BPP_FLOOR, BPP_MAX, 0.1);
    scale.set_draw_value(false);
    scale.set_round_digits(1);
    scale.set_width_request(220);
    scale.set_valign(gtk::Align::Center);
    let row = adw::ActionRow::builder()
        .title(spec::PYROWAVE_QUALITY.title())
        .visible(false)
        .build();
    row.add_suffix(&scale);
    let warning = adw::ActionRow::builder()
        .use_markup(false)
        .css_classes(["warning"])
        .visible(false)
        .build();
    warning.add_prefix(&gtk::Image::from_icon_name("dialog-warning-symbolic"));
    let shown = Rc::new(Cell::new(false));
    // Weak: the store outlives the dialog and keeps its listeners.
    let refresh: Rc<dyn Fn()> = {
        let (store, preset_id, native) = (Rc::downgrade(b.store), b.preset_id.clone(), b.native);
        let link = punktfunk_core::transport::ifinfo::local_link_facts(None);
        let (row, warning, scale, shown) = (
            row.downgrade(),
            warning.downgrade(),
            scale.downgrade(),
            shown.clone(),
        );
        Rc::new(move || {
            let (Some(store), Some(row), Some(warning), Some(scale)) = (
                store.upgrade(),
                row.upgrade(),
                warning.upgrade(),
                scale.upgrade(),
            ) else {
                return;
            };
            let globals = store.settings().clone();
            let preset = preset_id
                .as_ref()
                .and_then(|id| store.presets().find_by_id(id).cloned());
            let mut s = match preset {
                Some(p) => p.overrides.apply(&globals),
                None => globals,
            };
            s.pyrowave_bpp = scale.value();
            let (caption, warn) = s.pyrowave_quality_lines(native, link);
            set_row_subtitle(row.upcast_ref(), &caption);
            warning.set_title(warn.as_deref().unwrap_or(""));
            warning.set_visible(shown.get() && warn.is_some());
        })
    };
    {
        let (on_slide, on_store) = (refresh.clone(), refresh.clone());
        scale.connect_value_changed(move |_| on_slide());
        b.store.subscribe(move |_| on_store());
    }
    let show: Rc<dyn Fn(bool)> = {
        let (row, refresh) = (row.clone(), refresh.clone());
        Rc::new(move |on| {
            shown.set(on);
            row.set_visible(on);
            refresh();
        })
    };
    let s = scale.clone();
    let field = Field::new(
        &spec::PYROWAVE_QUALITY,
        &row,
        vec![row.clone().upcast(), warning.upcast()],
        {
            let s = s.clone();
            move |set| s.set_value(set.pyrowave_bpp)
        },
        {
            let s = s.clone();
            move |set| set.pyrowave_bpp = (s.value() * 10.0).round() / 10.0
        },
        {
            let s = s.clone();
            move || (s.value() - BPP_DEFAULT).abs() > 0.01
        },
        move |f| {
            s.connect_value_changed(move |_| f());
        },
    );
    (field, show)
}

/// The row's caption names the selected choice.
pub(super) fn caption_follows(row: &ChoiceRow, captions: &'static [Text]) {
    let w = row.widget().clone();
    row.connect_changed(move |i| set_row_subtitle(&w, at(captions, i)()));
}
