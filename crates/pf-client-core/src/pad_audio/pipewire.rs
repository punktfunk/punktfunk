//! Wired DualSense output on Linux: find the pad's PipeWire sink, move its card to a
//! four-channel profile for the session, pin the sink to unity gain, and play the mixed
//! stream on it. `mod.rs` builds this file as `usb` on Linux.

use super::{
    pick_pad_sink, pick_profile, props_say_ds5, CardDevice, CardProfile, PadSinkPick, SinkNode,
};
use crate::pw_oneshot::{OneShot, TIMEOUT};
use punktfunk_core::audio::pad_mix::PAD_CHANNELS;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// SDL `ConnectionState::Unknown` fallback: a DualSense sound card in the graph is the wired
/// signal (Bluetooth DS5 has none). Any profile counts, including stereo that cannot carry the
/// coils — `ensure_pro_audio` moves the profile and must not be gated on this.
pub(crate) fn wired_audio_sibling(_hid_path: Option<&str>) -> bool {
    match walk_graph() {
        Ok((sinks, cards)) => {
            cards.iter().any(|c| c.ds5) || sinks.iter().any(|s| s.ds5 && s.device_id.is_some())
        }
        Err(e) => {
            tracing::debug!(error = %format!("{e:#}"), "pad-audio wired probe: no PipeWire graph");
            false
        }
    }
}

/// Build a [`SinkNode`] from the node's INFO props, not the registry announce subset.
///
/// Here, not with the pure pickers in `mod.rs`: `DictRef` is `pipewire`, which the Windows
/// `lib test` target does not link.
pub(crate) fn sink_from_props(props: &pipewire::spa::utils::dict::DictRef) -> Option<SinkNode> {
    // `device.vendor.id` (PipeWire) or `vendor.id` (pulse/GE). Accept both.
    let vendor = props
        .get("device.vendor.id")
        .or_else(|| props.get("vendor.id"));
    let product = props
        .get("device.product.id")
        .or_else(|| props.get("product.id"));
    // `Audio/Sink` and `Audio/Sink/Internal` (hidden four-channel parent).
    let class = props.get("media.class")?;
    if !class.starts_with("Audio/Sink") {
        return None;
    }
    let name = props.get("node.name")?;
    let description = props
        .get("node.description")
        .or_else(|| props.get("node.nick"))
        .unwrap_or(name);
    let positions: Vec<String> = props
        .get("audio.position")
        .map(|p| {
            p.trim_matches(|c| c == '[' || c == ']')
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();
    Some(SinkNode {
        // Caller's `info.id()`; the proplist has no registry id.
        id: 0,
        device_id: props.get("device.id").and_then(|v| v.parse().ok()),
        channels: props
            .get("audio.channels")
            .and_then(|v| v.parse().ok())
            .unwrap_or(positions.len() as u32),
        split_parent: props.get("api.alsa.split.name").map(str::to_string),
        ds5: props_say_ds5(vendor, product, name, description),
        internal: class.ends_with("/Internal")
            || props
                .get("api.alsa.split.parent")
                .is_some_and(|v| !matches!(v, "false" | "0")),
        positions,
        name: name.to_string(),
        description: description.to_string(),
    })
}

/// Walk every `Audio/Sink…` node and `Device` in one bounded [`OneShot`] query.
///
/// Separate from [`crate::audio::devices`]: that walk publishes name + description only.
/// Two rounds: a registry `global` announce has `media.class` / `node.name` / `device.id`
/// but not `audio.channels` or `audio.position` (those live on the bound node's INFO).
/// Reading the announce yields 0 channels and a needless profile swap.
fn walk_graph() -> anyhow::Result<(Vec<SinkNode>, Vec<CardDevice>)> {
    use pipewire as pw;
    use std::cell::RefCell;
    use std::rc::Rc;

    let session = OneShot::connect("pad-graph", TIMEOUT)?;
    let sinks: Rc<RefCell<Vec<SinkNode>>> = Rc::default();
    let cards: Rc<RefCell<Vec<CardDevice>>> = Rc::default();
    // Proxies and listeners must outlive the callback that created them.
    let bound: Rc<RefCell<Vec<(pw::node::Node, pw::node::NodeListener)>>> = Rc::default();

    let _reg_listener = session
        .registry
        .add_listener_local()
        .global({
            let (registry, sinks, cards, bound) = (
                session.registry.clone(),
                sinks.clone(),
                cards.clone(),
                bound.clone(),
            );
            move |g| {
                let Some(props) = g.props else { return };
                match g.type_ {
                    pw::types::ObjectType::Node => {
                        // Announce is enough to classify a sink; matcher facts come from `info`.
                        if !props
                            .get("media.class")
                            .is_some_and(|c| c.starts_with("Audio/Sink"))
                        {
                            return;
                        }
                        let Ok(node) = registry.bind::<pw::node::Node, _>(g) else {
                            return;
                        };
                        let listener = node
                            .add_listener_local()
                            .info({
                                let sinks = sinks.clone();
                                move |info| {
                                    let Some(p) = info.props() else { return };
                                    if let Some(mut s) = sink_from_props(p) {
                                        s.id = info.id();
                                        let mut v = sinks.borrow_mut();
                                        // `info` can fire more than once; keep one entry per name.
                                        if let Some(old) = v.iter_mut().find(|o| o.name == s.name) {
                                            *old = s;
                                        } else {
                                            v.push(s);
                                        }
                                    }
                                }
                            })
                            .register();
                        bound.borrow_mut().push((node, listener));
                    }
                    pw::types::ObjectType::Device => {
                        // Cards announce identity keys; nothing else is weighed, so no bind.
                        let vendor = props
                            .get("device.vendor.id")
                            .or_else(|| props.get("vendor.id"));
                        let product = props
                            .get("device.product.id")
                            .or_else(|| props.get("product.id"));
                        let name = props.get("device.name").unwrap_or_default();
                        let description = props
                            .get("device.description")
                            .or_else(|| props.get("device.nick"))
                            .unwrap_or(name);
                        cards.borrow_mut().push(CardDevice {
                            id: g.id,
                            ds5: props_say_ds5(vendor, product, name, description),
                            name: name.to_string(),
                            description: description.to_string(),
                        });
                    }
                    _ => {}
                }
            }
        })
        .register();

    session.round()?; // 1: globals replay; sinks get bound
    session.round()?; // 2: the binds' `info` events land
    let out = (sinks.borrow().clone(), cards.borrow().clone());
    // Drop bound proxies before the core that owns them.
    bound.borrow_mut().clear();
    Ok(out)
}

/// Profile we moved and owe back. One card — v1 renders one DualSense.
#[derive(Clone, Copy, Debug)]
struct ProfileSwap {
    device_id: u32,
    previous: u32,
}

static PROFILE_SWAP: Mutex<Option<ProfileSwap>> = Mutex::new(None);

/// Cards already moved with no four-channel node. Retrying would flip the user's sound settings all session.
static PROFILE_TRIED: Mutex<Vec<u32>> = Mutex::new(Vec::new());

enum ProfileTarget {
    FourChannel,
    Index(u32),
}

/// Move the pad's card to a four-channel profile and remember the previous index.
///
/// User-visible, reverted at session end, `save = false` so WirePlumber does not remember it.
/// `PUNKTFUNK_PAD_AUDIO_PROFILE=0` leaves the card alone.
fn ensure_pro_audio(device_id: u32) -> anyhow::Result<()> {
    if crate::env_on("PUNKTFUNK_PAD_AUDIO_PROFILE") == Some(false) {
        anyhow::bail!(
            "the DualSense card has no four-channel profile active and \
             PUNKTFUNK_PAD_AUDIO_PROFILE=0 forbids moving it — switch the controller to \
             \"Pro Audio\" in your sound settings to feel haptics"
        );
    }
    let previous = set_card_profile(device_id, ProfileTarget::FourChannel)?;
    let mut swap = PROFILE_SWAP.lock().unwrap();
    // First swap is the user's setting; a later re-correlation must not restore our Pro Audio pick.
    if swap.is_none() {
        *swap = Some(ProfileSwap {
            device_id,
            previous,
        });
    }
    Ok(())
}

pub(super) fn restore_profile() {
    let Some(swap) = PROFILE_SWAP.lock().unwrap().take() else {
        return;
    };
    match set_card_profile(swap.device_id, ProfileTarget::Index(swap.previous)) {
        Ok(_) => tracing::info!(
            device = swap.device_id,
            profile = swap.previous,
            "DualSense card profile restored"
        ),
        // Unplug is the ordinary failure — the card (and the owed profile) is gone.
        Err(e) => tracing::debug!(
            error = %format!("{e:#}"),
            "DualSense card profile not restored (pad unplugged?)"
        ),
    }
}

/// Select a profile, returning the index it had. Three rounds under one deadline: registry
/// bind, then `enum_params`, then `set_param` — each waits on the previous replies.
fn set_card_profile(device_id: u32, want: ProfileTarget) -> anyhow::Result<u32> {
    use anyhow::{anyhow, Context};
    use pipewire as pw;
    use pw::spa::param::ParamType;
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    let session = OneShot::connect("pad-profile", TIMEOUT)?;
    let profiles: Rc<RefCell<Vec<CardProfile>>> = Rc::default();
    let active: Rc<Cell<Option<u32>>> = Rc::new(Cell::new(None));
    let device: Rc<RefCell<Option<pw::device::Device>>> = Rc::default();
    let dev_listener: Rc<RefCell<Option<pw::device::DeviceListener>>> = Rc::default();

    let _reg_listener = session
        .registry
        .add_listener_local()
        .global({
            let (registry, device, dev_listener) = (
                session.registry.clone(),
                device.clone(),
                dev_listener.clone(),
            );
            let (profiles, active) = (profiles.clone(), active.clone());
            move |g| {
                if g.id != device_id || g.type_ != pw::types::ObjectType::Device {
                    return;
                }
                let Ok(d) = registry.bind::<pw::device::Device, _>(g) else {
                    return;
                };
                let l = d
                    .add_listener_local()
                    .param({
                        let (profiles, active) = (profiles.clone(), active.clone());
                        move |_seq, id, _index, _next, param| {
                            let Some(p) = param.and_then(parse_profile) else {
                                return;
                            };
                            match id {
                                ParamType::EnumProfile => profiles.borrow_mut().push(p),
                                ParamType::Profile => active.set(Some(p.index)),
                                _ => {}
                            }
                        }
                    })
                    .register();
                *dev_listener.borrow_mut() = Some(l);
                *device.borrow_mut() = Some(d);
            }
        })
        .register();

    session.round()?; // registry replays globals; the card is bound
    {
        let d = device.borrow();
        let d = d
            .as_ref()
            .ok_or_else(|| anyhow!("card {device_id} is not in the PipeWire graph"))?;
        d.enum_params(0, Some(ParamType::EnumProfile), 0, u32::MAX);
        d.enum_params(1, Some(ParamType::Profile), 0, 1);
    }
    session.round()?; // EnumProfile + active Profile

    let previous = active
        .get()
        .ok_or_else(|| anyhow!("card {device_id} did not report an active profile"))?;
    // Scope the borrow: round 3 re-enters the mainloop and the param listener may re-emit EnumProfile.
    let pick = {
        let list = profiles.borrow();
        match want {
            ProfileTarget::Index(i) => i,
            ProfileTarget::FourChannel => {
                let p = pick_profile(&list).ok_or_else(|| {
                    anyhow!(
                        "the DualSense card offers no four-channel profile ({} enumerated: {})",
                        list.len(),
                        list.iter()
                            .map(|p| p.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                })?;
                tracing::info!(
                    profile = %p.name,
                    description = %p.description,
                    was = previous,
                    "moving the DualSense card to a four-channel profile so the voice coils are \
                     reachable (restored when the session ends)"
                );
                p.index
            }
        }
    };
    if pick == previous {
        return Ok(previous);
    }
    let pod = profile_pod(pick).context("serialize Profile pod")?;
    {
        let d = device.borrow();
        let d = d
            .as_ref()
            .ok_or_else(|| anyhow!("card {device_id} vanished mid-swap"))?;
        d.set_param(
            ParamType::Profile,
            0,
            pw::spa::pod::Pod::from_bytes(&pod).ok_or_else(|| anyhow!("bad Profile pod"))?,
        );
    }
    session.round()?; // flush set_param before the loop and its proxies drop
    Ok(previous)
}

fn parse_profile(pod: &pipewire::spa::pod::Pod) -> Option<CardProfile> {
    use pipewire::spa::pod::{deserialize::PodDeserializer, Value};
    // `SPA_PARAM_AVAILABILITY_no` — the only availability that cannot be selected.
    const AVAILABILITY_NO: u32 = 1;
    let (_, value) = PodDeserializer::deserialize_any_from(pod.as_bytes()).ok()?;
    let Value::Object(obj) = value else {
        return None;
    };
    let mut p = CardProfile {
        available: true,
        ..CardProfile::default()
    };
    for prop in obj.properties {
        match (prop.key, prop.value) {
            (pipewire::spa::sys::SPA_PARAM_PROFILE_index, Value::Int(i)) => p.index = i as u32,
            (pipewire::spa::sys::SPA_PARAM_PROFILE_name, Value::String(s)) => p.name = s,
            (pipewire::spa::sys::SPA_PARAM_PROFILE_description, Value::String(s)) => {
                p.description = s
            }
            (pipewire::spa::sys::SPA_PARAM_PROFILE_available, Value::Id(id)) => {
                p.available = id.0 != AVAILABILITY_NO
            }
            _ => {}
        }
    }
    Some(p)
}

/// Profile pod for `index`. `save = false`: session borrow, not a WirePlumber preference.
fn profile_pod(index: u32) -> anyhow::Result<Vec<u8>> {
    use anyhow::Context;
    use pipewire::spa;
    use spa::pod::{Object, Property, PropertyFlags, Value};
    let obj = Object {
        type_: spa::utils::SpaTypes::ObjectParamProfile.as_raw(),
        id: spa::param::ParamType::Profile.as_raw(),
        properties: vec![
            Property {
                key: spa::sys::SPA_PARAM_PROFILE_index,
                flags: PropertyFlags::empty(),
                value: Value::Int(index as i32),
            },
            Property {
                key: spa::sys::SPA_PARAM_PROFILE_save,
                flags: PropertyFlags::empty(),
                value: Value::Bool(false),
            },
        ],
    };
    Ok(spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &Value::Object(obj),
    )
    .context("serialize")?
    .0
    .into_inner())
}

/// Unity gain: 1.0 in `channelVolumes`. Pulse UIs cube the scale, so WirePlumber's 0.4 default
/// is 0.4³ ≈ −24 dB in linear units. 1.0 is unity on both scales.
fn unity_volume_pod(channels: u32) -> anyhow::Result<Vec<u8>> {
    use anyhow::Context;
    use pipewire::spa;
    use spa::pod::{Object, Property, PropertyFlags, Value, ValueArray};
    let obj = Object {
        type_: spa::utils::SpaTypes::ObjectParamProps.as_raw(),
        id: spa::param::ParamType::Props.as_raw(),
        properties: vec![
            Property {
                key: spa::sys::SPA_PROP_volume,
                flags: PropertyFlags::empty(),
                value: Value::Float(1.0),
            },
            Property {
                key: spa::sys::SPA_PROP_channelVolumes,
                flags: PropertyFlags::empty(),
                value: Value::ValueArray(ValueArray::Float(vec![1.0; channels.max(1) as usize])),
            },
        ],
    };
    Ok(spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &Value::Object(obj),
    )
    .context("serialize")?
    .0
    .into_inner())
}

/// Pin the pad sink to unity. WirePlumber starts new cards at 0.4 (−24 dB, cubed UI 40%)
/// globally, so both session ends stack. Not restored: putting −24 dB back would restore the
/// bug. Failures cost attenuation, never audio. `PUNKTFUNK_PAD_SINK_VOLUME=0` skips.
/// Two rounds under one deadline: registry bind, then `set_param`.
fn pin_sink_volume(node_id: u32, channels: u32) -> anyhow::Result<()> {
    use anyhow::{anyhow, Context};
    use pipewire as pw;
    use std::cell::RefCell;
    use std::rc::Rc;

    let session = OneShot::connect("pad-volume", TIMEOUT)?;
    let node: Rc<RefCell<Option<pw::node::Node>>> = Rc::default();
    let _reg_listener = session
        .registry
        .add_listener_local()
        .global({
            let (registry, node) = (session.registry.clone(), node.clone());
            move |g| {
                if g.id != node_id || g.type_ != pw::types::ObjectType::Node {
                    return;
                }
                if let Ok(n) = registry.bind::<pw::node::Node, _>(g) {
                    *node.borrow_mut() = Some(n);
                }
            }
        })
        .register();

    session.round()?; // registry replays globals; the node is bound
    let pod = unity_volume_pod(channels).context("serialize Props pod")?;
    {
        let n = node.borrow();
        let n = n
            .as_ref()
            .ok_or_else(|| anyhow!("sink node {node_id} is not in the PipeWire graph"))?;
        n.set_param(
            pw::spa::param::ParamType::Props,
            0,
            pw::spa::pod::Pod::from_bytes(&pod).ok_or_else(|| anyhow!("bad Props pod"))?,
        );
    }
    session.round()?; // flush set_param before the loop and its proxies drop
    Ok(())
}

/// Pin the picked node to unity on every (re)correlation — a profile change remints nodes.
fn pin_picked(name: String, sinks: &[SinkNode]) -> String {
    if crate::env_on("PUNKTFUNK_PAD_SINK_VOLUME") == Some(false) {
        return name;
    }
    // `split_parent` is a name on another node's proplist; there may be no bindable object.
    let Some(s) = sinks.iter().find(|s| s.name == name && s.id != 0) else {
        return name;
    };
    match pin_sink_volume(s.id, s.channels) {
        Ok(()) => tracing::debug!(node = %name, channels = s.channels, "pad sink pinned to 0 dB"),
        Err(e) => tracing::debug!(
            node = %name,
            error = %format!("{e:#}"),
            "could not pin the pad sink to 0 dB — haptics may be quiet if the session manager \
             left it at its default 40%"
        ),
    }
    name
}

pub fn correlate_pad_sink() -> anyhow::Result<String> {
    use anyhow::anyhow;
    let (sinks, cards) = walk_graph()?;
    match pick_pad_sink(&sinks, &cards) {
        Some(PadSinkPick::Node(name)) => Ok(pin_picked(name, &sinks)),
        Some(PadSinkPick::NeedsProfile(device_id)) => {
            if PROFILE_TRIED.lock().unwrap().contains(&device_id) {
                return Err(anyhow!(
                    "the DualSense card has no four-channel node and moving its profile did \
                     not help earlier this session — not moving it again"
                ));
            }
            ensure_pro_audio(device_id)?;
            // Profile change remints nodes; wait ~2 s rather than the caller's multi-second backoff.
            let mut last = Vec::new();
            for _ in 0..20 {
                std::thread::sleep(Duration::from_millis(100));
                let (sinks, cards) = walk_graph()?;
                if let Some(PadSinkPick::Node(name)) = pick_pad_sink(&sinks, &cards) {
                    return Ok(pin_picked(name, &sinks));
                }
                last = sinks;
            }
            // Swap was pure cost: restore now, and do not move this card again.
            PROFILE_TRIED.lock().unwrap().push(device_id);
            restore_profile();
            // 0 channels and stereo look the same here. A sandbox (flatpak) often cannot
            // `set_param` on a device it does not own — no error reply, so this is where it shows.
            Err(anyhow!(
                "the DualSense card has no four-channel node, and moving its profile did not \
                 produce one — set the controller's Profile to \"Pro Audio\" in your sound \
                 settings (a sandboxed client may not be allowed to do it for you). Its sinks \
                 are [{}] (run `punktfunk-session --pad-audio-test` for the full graph)",
                last.iter()
                    .filter(|s| s.ds5)
                    .map(|s| format!("{}={}ch", s.name, s.channels))
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        }
        None => Err(anyhow!("no DualSense sound card in the PipeWire graph")),
    }
}

/// `punktfunk-session --pad-audio-test`: print correlation, then a tone so silence is
/// "nothing arrives" vs "graph folded it" vs "firmware muted". 200 Hz on the coil pair
/// (channels 3/4), speaker silent unless asked. Buzz means everything below the plane is good.
pub fn pad_audio_test(seconds: u64, coils: bool, speaker: bool) -> anyhow::Result<()> {
    // Restore even on `?` — otherwise a failed test leaves the card on Pro Audio.
    let out = pad_audio_test_inner(seconds, coils, speaker);
    restore_profile();
    out
}

fn pad_audio_test_inner(seconds: u64, coils: bool, speaker: bool) -> anyhow::Result<()> {
    let (sinks, cards) = walk_graph()?;
    // Totals first: "no DualSense" and "walk saw nothing" both print an empty list.
    println!(
        "== DualSense objects in the PipeWire graph (of {} sinks, {} cards) ==",
        sinks.len(),
        cards.len()
    );
    for c in cards.iter().filter(|c| c.ds5) {
        println!("card   id={:<5} {}  ({})", c.id, c.name, c.description);
    }
    for s in sinks.iter().filter(|s| {
        s.ds5
            || s.device_id
                .is_some_and(|id| cards.iter().any(|c| c.id == id && c.ds5))
    }) {
        println!(
            "{:<7} device.id={:<7} channels={} position={:<24} {}{}",
            if s.internal { "parent" } else { "sink" },
            s.device_id
                .map(|d| d.to_string())
                .unwrap_or_else(|| "-(virtual)".into()),
            s.channels,
            if s.positions.is_empty() {
                "-".into()
            } else {
                s.positions.join(",")
            },
            s.name,
            s.split_parent
                .as_deref()
                .map(|p| format!("  split.parent={p}"))
                .unwrap_or_default(),
        );
    }
    match pick_pad_sink(&sinks, &cards) {
        None => {
            println!("\nno DualSense sound card found — is the pad plugged in over USB?");
            anyhow::bail!("no DualSense sound card in the PipeWire graph");
        }
        Some(PadSinkPick::Node(n)) => println!("\npick: render on {n}"),
        Some(PadSinkPick::NeedsProfile(d)) => println!(
            "\npick: card {d} has no four-channel node — moving it to a four-channel profile"
        ),
    }

    let out = PadOut::open()?;
    println!(
        "playing {seconds}s: {} — the coils are channels 3/4, the speaker 1/2",
        match (coils, speaker) {
            (true, true) => "a tone on BOTH pairs",
            (true, false) => "a tone on the voice coils only",
            (false, true) => "a tone on the speaker only",
            (false, false) => "silence (both pairs off)",
        }
    );
    // 200 Hz at 0.5: coils move air rather than click. 480-frame (10 ms) chunks, wall-clock paced.
    let mut phase = 0f32;
    let step = std::f32::consts::TAU * 200.0 / 48_000.0;
    let deadline = Instant::now() + Duration::from_secs(seconds);
    while Instant::now() < deadline {
        let mut chunk = out.take_buffer();
        chunk.clear();
        for _ in 0..480 {
            let s = phase.sin() * 0.5;
            phase = (phase + step) % std::f32::consts::TAU;
            let sp = if speaker { s } else { 0.0 };
            let co = if coils { s } else { 0.0 };
            chunk.extend_from_slice(&[sp, sp, co, co]);
        }
        out.push(chunk);
        std::thread::sleep(Duration::from_millis(10));
    }
    drop(out);
    Ok(())
}

/// `finished()` is device-gone — the worker drops and re-correlates.
pub(super) struct PadOut {
    pcm_tx: std::sync::mpsc::SyncSender<Vec<f32>>,
    recycle_rx: std::sync::mpsc::Receiver<Vec<f32>>,
    quit_tx: pipewire::channel::Sender<()>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl PadOut {
    pub(super) fn open() -> anyhow::Result<PadOut> {
        use anyhow::Context;
        let target = correlate_pad_sink()?;
        tracing::info!(sink = %target, "pad-audio sink matched");
        // 64 × 5 ms slack; recycle pool keeps steady state allocation-free.
        let (pcm_tx, pcm_rx) = std::sync::mpsc::sync_channel::<Vec<f32>>(64);
        let (recycle_tx, recycle_rx) = std::sync::mpsc::sync_channel::<Vec<f32>>(64);
        let (quit_tx, quit_rx) = pipewire::channel::channel::<()>();
        let thread = std::thread::Builder::new()
            .name("pf-pad-audio-out".into())
            .spawn(move || {
                // `process` runs here (no RT_PROCESS); this thread has to make the pad's device cycles.
                crate::audio_rt::boost_and_log("pf-pad-audio-out");
                if let Err(e) = pad_pw_thread(pcm_rx, recycle_tx, quit_rx, target) {
                    tracing::warn!(error = %format!("{e:#}"), "pad-audio playback thread ended");
                }
            })
            .context("spawn pad-audio playback thread")?;
        Ok(PadOut {
            pcm_tx,
            recycle_rx,
            quit_tx,
            thread: Some(thread),
        })
    }

    pub(super) fn take_buffer(&self) -> Vec<f32> {
        self.recycle_rx.try_recv().unwrap_or_default()
    }

    pub(super) fn push(&self, pcm: Vec<f32>) {
        let _ = self.pcm_tx.try_send(pcm); // never block the renderer; drops are concealed
    }

    pub(super) fn finished(&self) -> bool {
        self.thread.as_ref().is_none_or(|t| t.is_finished())
    }
}

impl Drop for PadOut {
    fn drop(&mut self) {
        let _ = self.quit_tx.send(());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Playback on the DualSense node: 4 unpositioned `AUX0..AUX3`, 5 ms quantum, ring floor
/// 3 quanta in [240, 2400] frames (main player is [720, 9600] — haptics are felt latency).
fn pad_pw_thread(
    pcm_rx: std::sync::mpsc::Receiver<Vec<f32>>,
    recycle_tx: std::sync::mpsc::SyncSender<Vec<f32>>,
    quit_rx: pipewire::channel::Receiver<()>,
    target: String,
) -> anyhow::Result<()> {
    use anyhow::Context;
    use pipewire as pw;
    use pw::{properties::properties, spa};
    use spa::param::audio::{AudioFormat, AudioInfoRaw};
    use spa::pod::Pod;

    pw::init();

    let mainloop = pw::main_loop::MainLoopRc::new(None).context("pw MainLoop")?;
    let context = pw::context::ContextRc::new(&mainloop, None).context("pw Context")?;
    let core = context
        .connect_rc(None)
        .context("pw connect (is PipeWire running in this session?)")?;

    let _quit_guard = quit_rx.attach(mainloop.loop_(), {
        let mainloop = mainloop.clone();
        move |_| mainloop.quit()
    });

    let props = properties! {
        *pw::keys::MEDIA_TYPE       => "Audio",
        *pw::keys::MEDIA_CATEGORY   => "Playback",
        *pw::keys::MEDIA_ROLE       => "Game",
        *pw::keys::NODE_NAME        => "punktfunk-pad-audio",
        *pw::keys::NODE_DESCRIPTION => "Punktfunk Pad Audio",
        // ~5 ms quantum (one haptics Opus frame) keeps felt latency small.
        *pw::keys::NODE_LATENCY     => "240/48000",
        // Raw key: `keys::TARGET_OBJECT` is feature-gated on a newer libpipewire than we require.
        "target.object"             => target.as_str(),
        // Unplug must END the stream (worker re-correlates), not re-route 4-ch haptics to desktop speakers.
        "node.dont-reconnect"       => "true",
        // Without this the graph position-remixes the quad and folds coils into the speaker.
        // Channel k → channel k is the only map the firmware understands.
        *pw::keys::STREAM_DONT_REMIX => "true",
    };
    let stream =
        pw::stream::StreamBox::new(&core, "punktfunk-pad-audio", props).context("pw Stream")?;

    struct PadPlayData {
        rx: std::sync::mpsc::Receiver<Vec<f32>>,
        recycle: std::sync::mpsc::SyncSender<Vec<f32>>,
        ring: std::collections::VecDeque<f32>,
        primed: bool,
    }
    let ud = PadPlayData {
        rx: pcm_rx,
        recycle: recycle_tx,
        ring: std::collections::VecDeque::new(),
        primed: false,
    };

    let _listener = stream
        .add_local_listener_with_user_data(ud)
        .state_changed({
            let mainloop = mainloop.clone();
            move |_s, _ud, old, new| {
                tracing::debug!(?old, ?new, "pipewire pad-audio stream state");
                // Unplug + dont-reconnect → Error. Quit so `finished()` re-correlates.
                if matches!(new, pw::stream::StreamState::Error(_)) {
                    mainloop.quit();
                }
            }
        })
        .process(|stream, ud| {
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let Some(mut buffer) = stream.dequeue_buffer() else {
                    return;
                };
                while let Ok(mut chunk) = ud.rx.try_recv() {
                    ud.ring.extend(chunk.iter().copied());
                    chunk.clear();
                    let _ = ud.recycle.try_send(chunk);
                }
                // This cycle's quantum, as in the playback stream: the mapped buffer is sized
                // for `quantum-limit` (8192 ≈ 170 ms), which would lag every hit. 0 → capacity.
                let requested = usize::try_from(buffer.requested()).unwrap_or(0);
                let stride = 4 * PAD_CHANNELS; // F32LE interleaved
                let datas = buffer.datas_mut();
                if datas.is_empty() {
                    return;
                }
                let data = &mut datas[0];
                let max_frames = data.data().map(|s| s.len() / stride).unwrap_or(0);
                let want_frames = match requested {
                    0 => max_frames,
                    r => r.min(max_frames),
                };
                let want = want_frames * PAD_CHANNELS;

                // Prime ~3 quanta in [240, 2400] frames; cap ~1 quantum of slack; re-prime after a drain.
                let target = (3 * want).clamp(240 * PAD_CHANNELS, 2400 * PAD_CHANNELS);
                while ud.ring.len() > target.max(want) + want {
                    ud.ring.pop_front();
                }
                if !ud.primed && ud.ring.len() >= target {
                    ud.primed = true;
                }

                let n_frames = if let Some(slice) = data.data() {
                    for k in 0..want {
                        let s = if ud.primed {
                            ud.ring.pop_front().unwrap_or(0.0)
                        } else {
                            0.0
                        };
                        let off = k * 4;
                        slice[off..off + 4].copy_from_slice(&s.to_le_bytes());
                    }
                    want_frames
                } else {
                    0
                };
                if ud.ring.is_empty() {
                    ud.primed = false;
                }
                let chunk = data.chunk_mut();
                *chunk.offset_mut() = 0;
                *chunk.stride_mut() = stride as _;
                *chunk.size_mut() = (stride * n_frames) as _;
            }));
            if outcome.is_err() {
                tracing::error!("panic in pipewire pad-audio callback");
            }
        })
        .register()
        .context("register pad-audio listener")?;

    let mut info = AudioInfoRaw::new();
    info.set_format(AudioFormat::F32LE);
    info.set_rate(48_000);
    info.set_channels(PAD_CHANNELS as u32);
    // AUX0..AUX3 (`SPA_AUDIO_CHANNEL_START_Aux` = 0x1000), not FL FR RL RR. Aux has no
    // spatial meaning, so Pro Audio / split parents / GE-Proton / our host sink agree.
    // `stream.dont-remix` already index-routes; this removes a position to remix against.
    const AUX0: u32 = 0x1000;
    let mut positions = [0u32; 64];
    positions[..4].copy_from_slice(&[AUX0, AUX0 + 1, AUX0 + 2, AUX0 + 3]);
    info.set_position(positions);
    let obj = pw::spa::pod::Object {
        type_: pw::spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: pw::spa::param::ParamType::EnumFormat.as_raw(),
        properties: info.into(),
    };
    let values: Vec<u8> = pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(obj),
    )
    .context("serialize pad format pod")?
    .0
    .into_inner();
    let mut params = [Pod::from_bytes(&values).context("pad pod from bytes")?];

    stream
        .connect(
            spa::utils::Direction::Output,
            None,
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .context("pw pad stream connect")?;

    mainloop.run();
    tracing::debug!("pipewire pad-audio loop exited");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `channelVolumes` length must match the port count; a mismatch is ignored and looks like the pin silently failed.
    #[test]
    fn unity_pod_is_one_float_per_channel() {
        use pipewire::spa::pod::{deserialize::PodDeserializer, Value, ValueArray};
        for channels in [1u32, 2, 4] {
            let bytes = unity_volume_pod(channels).expect("serialize");
            let (_, value) = PodDeserializer::deserialize_any_from(&bytes).expect("parse");
            let Value::Object(obj) = value else {
                panic!("not an object pod");
            };
            let vols = obj
                .properties
                .iter()
                .find(|p| p.key == pipewire::spa::sys::SPA_PROP_channelVolumes)
                .map(|p| p.value.clone())
                .expect("channelVolumes");
            let Value::ValueArray(ValueArray::Float(v)) = vols else {
                panic!("channelVolumes is not a float array");
            };
            assert_eq!(v.len(), channels as usize);
            assert!(v.iter().all(|&x| x == 1.0), "every channel must be unity");
        }
    }
}
