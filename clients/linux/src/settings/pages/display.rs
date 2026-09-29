//! Display: the stream's size and rate, the picture, and the advanced encode and present rows.

use super::{Build, PageB};
use crate::settings::choice::{set_row_subtitle, ChoiceRow};
use crate::settings::field::Field;
use crate::settings::spec::{self, Page};
use crate::settings::tables::*;
use crate::trust::Settings;
use adw::prelude::*;
use std::cell::Cell;
use std::rc::Rc;

pub fn display(b: &mut Build) {
    let mut p = b.page(Page::Display, "video-display-symbolic");
    let size = p.group("Resolution", "");
    let res = resolution(b);
    b.put(&mut p, Some(&size), res);
    let names: Vec<String> = REFRESH
        .iter()
        .map(|&r| match r {
            0 => "Native".to_string(),
            r => format!("{r} Hz"),
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
    let picture = p.group("Picture", "Display changes apply from the next session.");
    let (bitrate_field, bitrate) = bitrate_row();
    b.put(&mut p, Some(&picture), bitrate_field);
    let fit = b.choice(&spec::VIDEO_FIT, VIDEO_FIT_LABELS);
    caption_follows(&fit, VIDEO_FIT_CAPTIONS);
    b.put(
        &mut p,
        Some(&picture),
        Field::choice(&spec::VIDEO_FIT, &fit, index::video_fit, |s, i| {
            s.video_fit = at(VIDEO_FITS, i).to_string()
        }),
    );
    let (field, _) = Field::switch(&spec::HDR, |s| s.hdr_enabled, |s, v| s.hdr_enabled = v);
    b.put(&mut p, Some(&picture), field);
    // The buffer only means something under Smoothness, so it hides the rest of the time.
    let present = b.choice(&spec::PRESENT, PRESENT_PRIORITY_LABELS);
    let buffer = b.choice(&spec::SMOOTH_BUFFER, SMOOTH_BUFFER_LABELS);
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
            |s, i| s.smooth_buffer = i.min(SMOOTH_BUFFER_LABELS.len() as u32 - 1) as u8,
        ),
    );
    let names: Vec<String> = RENDER_SCALES
        .iter()
        .map(|&s| render_scale_label(s))
        .collect();
    let row = b.choice(
        &spec::RENDER_SCALE,
        &names.iter().map(String::as_str).collect::<Vec<_>>(),
    );
    b.put(
        &mut p,
        None,
        Field::choice(&spec::RENDER_SCALE, &row, index::render_scale, |s, i| {
            s.render_scale = *at(&RENDER_SCALES, i)
        }),
    );
    let codec = b.choice(&spec::CODEC, CODEC_LABELS);
    {
        let w = codec.widget().clone();
        codec.connect_changed(move |i| {
            set_row_subtitle(&w, codec_caption(i));
            lock_bitrate(&bitrate, *at(CODECS, i) == "pyrowave");
        });
    }
    b.put(
        &mut p,
        None,
        Field::choice(&spec::CODEC, &codec, index::codec, |s, i| {
            s.codec = at(CODECS, i).to_string()
        }),
    );
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
            "Automatic",
            "KWin",
            "Mutter (GNOME)",
            "Hyprland",
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

/// Decoder and GPU: facts about this device's hardware, never a preset's.
fn hardware(b: &mut Build, p: &mut PageB) {
    let row = b.choice(
        &spec::DECODER,
        &["Automatic", "Vulkan Video", "VAAPI", "Software"],
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
    let mut names = vec!["Automatic".to_string()];
    let mut keys = vec![String::new()];
    for a in &b.probes.adapters {
        names.push(a.clone());
        keys.push(a.clone());
    }
    if !saved.is_empty() && !keys.contains(&saved) {
        names.push(format!("{saved} (not detected)"));
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
    let width = size_row("Width", punktfunk_core::resolutions::MIN_WIDTH);
    let height = size_row("Height", punktfunk_core::resolutions::MIN_HEIGHT);
    let aspect = ChoiceRow::new(
        &b.dialog,
        b.inline,
        "Aspect ratio",
        "Which shapes the Resolution row offers",
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

/// A Width or Height row for a typed size, hidden until Resolution is on Custom.
fn size_row(title: &str, min: u32) -> adw::SpinRow {
    let row = adw::SpinRow::with_range(f64::from(min), 8192.0, 2.0);
    row.set_title(title);
    row.set_visible(false);
    row
}

/// 1 Mbit/s per step: the rungs that matter on a thin link are 3, 4, 6.
fn bitrate_row() -> (Field, adw::SpinRow) {
    let row = adw::SpinRow::with_range(0.0, 3000.0, 1.0);
    row.set_title(spec::BITRATE.title);
    row.set_subtitle(spec::BITRATE.caption);
    let r = row.clone();
    let field = Field::new(
        &spec::BITRATE,
        &row,
        vec![row.clone().upcast()],
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
    (field, row)
}

/// Under PyroWave the host sets the rate from the stream mode: the Bitrate row greys out and
/// says so. The stored rate stays for the other codecs.
fn lock_bitrate(row: &adw::SpinRow, pyrowave: bool) {
    row.set_sensitive(!pyrowave);
    set_row_subtitle(
        row.upcast_ref(),
        if pyrowave {
            "PyroWave sets its own rate from the stream mode"
        } else {
            BITRATE_CAPTION
        },
    );
}

/// The row's caption names the selected choice.
pub(super) fn caption_follows(row: &ChoiceRow, captions: &'static [&'static str]) {
    let w = row.widget().clone();
    row.connect_changed(move |i| set_row_subtitle(&w, at(captions, i)));
}
