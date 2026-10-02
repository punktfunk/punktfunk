//! DualSense haptics and speaker (`0xD1`) on a wired pad's four-channel USB audio
//! device. Bluetooth pads have no audio sibling.
//!
//! [`spawn`] correlates the pad with its WASAPI or PipeWire endpoint, decodes both
//! Opus streams with gap concealment, and interleaves speaker on 0/1 and voice coils
//! on 2/3. Linux needs the card's four-channel profile, index-based `AUX0..AUX3`
//! mapping, and exclusion of host-minted look-alike sinks; `ensure_pro_audio` moves
//! the profile for the session and restores it on exit.
//!
//! `pad_haptics` and `pad_speaker` gate capability advertisement. Speaker `"mix"`
//! is not implemented and behaves as `"off"`.

use punktfunk_core::audio::pad_mix::{plc_frames, HapticsLiveness, QuadMixer, PAD_CHANNELS};
use punktfunk_core::audio::AudioGapTracker;
use punktfunk_core::client::NativeClient;
use punktfunk_core::quic::{PAD_AUDIO_KIND_HAPTICS, PAD_AUDIO_KIND_SPEAKER};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// 4800 frames = 100 ms @ 48 kHz. Caps a wedged/absent output; live latency is the platform ring.
const MAX_BUFFER_FRAMES: usize = 4800;

/// Correlation walks the audio graph; poll a missing device from 1 s doubling to 8 s, not per frame.
const RETRY_MIN: Duration = Duration::from_secs(1);
const RETRY_MAX: Duration = Duration::from_secs(8);

/// `"pad"` opens the physical speaker. `"mix"` is unimplemented and treated as `"off"` so the name can ship.
pub fn speaker_active(mode: &str) -> bool {
    match mode {
        "pad" => true,
        "mix" => {
            static ONCE: std::sync::Once = std::sync::Once::new();
            ONCE.call_once(|| {
                tracing::info!(
                    "pad_speaker=\"mix\" is not implemented yet (TODO: fold the DS5 speaker \
                     stream into the main session audio) — treating it as \"off\""
                );
            });
            false
        }
        _ => false,
    }
}

/// Wired DualSense / DualSense Edge only: USB exposes the 4-ch audio device; Bluetooth does not.
/// `wired` is `SDL_GetGamepadConnectionState`, else [`wired_audio_sibling`] when SDL says Unknown.
pub(crate) fn is_tier_a_ds5(vid: u16, pid: u16, wired: bool) -> bool {
    vid == 0x054C && matches!(pid, 0x0CE6 | 0x0DF2) && wired
}

/// SDL `ConnectionState::Unknown` fallback: a DualSense sound card in the graph is the wired
/// signal (Bluetooth DS5 has none). Any profile counts, including stereo that cannot carry the
/// coils — `ensure_pro_audio` moves the profile and must not be gated on this.
#[cfg(target_os = "linux")]
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

#[cfg(windows)]
pub(crate) fn wired_audio_sibling(hid_path: Option<&str>) -> bool {
    hid_path.is_some_and(|p| correlate_pad_endpoint(p).is_ok())
}

struct TierAPad {
    index: u8,
    /// Windows correlation only; Linux matches the sink by signature.
    #[cfg_attr(not(windows), allow(dead_code))]
    hid_path: Option<String>,
}

/// Shared by the app-lifetime gamepad worker (write at slot open/close) and the per-session
/// renderer (read at correlation). Process-wide because the two workers share no other path.
static TIER_A_PADS: Mutex<Vec<TierAPad>> = Mutex::new(Vec::new());

pub(crate) fn register_tier_a(index: u8, hid_path: Option<String>) {
    let mut pads = TIER_A_PADS.lock().unwrap();
    pads.retain(|p| p.index != index);
    pads.push(TierAPad { index, hid_path });
}

pub(crate) fn unregister_tier_a(index: u8) {
    TIER_A_PADS.lock().unwrap().retain(|p| p.index != index);
}

/// Last rendered haptics frame per wire pad.
static HAPTICS: HapticsLiveness = HapticsLiveness::new();

/// Slot teardown: wire indices are reused, and a stale stamp would take the next pad's rumble.
pub(crate) fn clear_haptics_liveness(pad: u8) {
    HAPTICS.clear(pad);
}

/// Whether haptics frames drive `pad`'s coils right now, so wire rumble must stand down. Judged
/// on arrival: a title that renders no haptics audio keeps its rumble.
pub(crate) fn haptics_live(pad: u8) -> bool {
    HAPTICS.live(pad)
}

/// First registered pad's HID path — v1 renders one DualSense.
#[cfg(windows)]
fn first_tier_a_hid_path() -> Option<String> {
    TIER_A_PADS
        .lock()
        .unwrap()
        .first()
        .and_then(|p| p.hid_path.clone())
}

/// Weaker DualSense identity: name/description when the proplist has no USB ids.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn is_ds5_sink(name: &str, description: &str) -> bool {
    let hit = |s: &str| {
        s.contains("Sony_Interactive_Entertainment")
            || s.contains("DualSense")
            || s.starts_with("Wireless Controller")
    };
    hit(name) || hit(description)
}

/// DualSense / DualSense Edge USB ids — same pair GE-Proton matches.
#[cfg(any(target_os = "linux", test))]
const DS5_VENDOR: u32 = 0x054C;
#[cfg(any(target_os = "linux", test))]
const DS5_PRODUCTS: [u32; 2] = [0x0CE6, 0x0DF2];

/// PipeWire USB ids are hex, with or without `0x`. Decimal parse of `"0994"` would succeed and be wrong.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn parse_usb_id(v: &str) -> Option<u32> {
    let v = v.trim();
    let hex = v
        .strip_prefix("0x")
        .or_else(|| v.strip_prefix("0X"))
        .unwrap_or(v);
    u32::from_str_radix(hex, 16).ok()
}

#[cfg(any(target_os = "linux", test))]
pub(crate) fn props_say_ds5(
    vendor: Option<&str>,
    product: Option<&str>,
    name: &str,
    description: &str,
) -> bool {
    let ids = vendor.and_then(parse_usb_id) == Some(DS5_VENDOR)
        && product
            .and_then(parse_usb_id)
            .is_some_and(|p| DS5_PRODUCTS.contains(&p));
    ids || is_ds5_sink(name, description)
}

#[cfg(any(target_os = "linux", test))]
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct SinkNode {
    /// Registry global id for [`pin_sink_volume`]. `0` = no walk produced this node (fixtures, named-but-unseen split parent).
    pub(crate) id: u32,
    /// `node.name` — stream `target.object`.
    pub(crate) name: String,
    pub(crate) description: String,
    /// Card this node belongs to. `None` is a host-minted pad sink (full DualSense identity, no card) — skip it.
    pub(crate) device_id: Option<u32>,
    pub(crate) channels: u32,
    pub(crate) positions: Vec<String>,
    /// `api.alsa.split.name` — hidden four-channel parent of a split card. GE-Proton's haptic target.
    pub(crate) split_parent: Option<String>,
    /// This node's own proplist said DualSense. `pick_pad_sink` also accepts the card.
    pub(crate) ds5: bool,
    /// Hidden raw parent (`Audio/Sink/Internal`). Last four-channel choice — AUX0 is dead on that node.
    pub(crate) internal: bool,
}

#[cfg(any(target_os = "linux", test))]
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct CardDevice {
    pub(crate) id: u32,
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) ds5: bool,
}

#[cfg(any(target_os = "linux", test))]
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum PadSinkPick {
    /// Four-channel DualSense node (or the split parent a public sink names).
    Node(String),
    /// Pad present, no four-channel node. `device.id` to move.
    NeedsProfile(u32),
}

/// `AUX*` / unknown maps are index-routed. `FL,FR,RL,RR` is positioned and only works with `stream.dont-remix`.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn is_unpositioned(positions: &[String]) -> bool {
    positions.is_empty()
        || positions.iter().all(|p| {
            let p = p.trim();
            p.is_empty() || p.starts_with("AUX") || p == "UNK" || p == "NA"
        })
}

/// Pick the DualSense four-channel node, or the card whose profile is in the way.
///
/// Coils are channels 3 and 4; a stereo/mono node opens and dumps haptics into the
/// headphone jack. First match wins (v1: one pad).
///
/// Public four-channel sinks beat the hidden `Audio/Sink/Internal` parent. On UCM
/// split cards the parent is `AUX0..AUX3` with AUX0 dead / AUX1 = speaker, so
/// index-exact speaker-on-0/1 throws away the left speaker. The public
/// `SpeakerHaptic` sink folds 0/1 onto AUX1 and passes the coils. Pro Audio's
/// public `AUX` quad is caught first.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn pick_pad_sink(sinks: &[SinkNode], cards: &[CardDevice]) -> Option<PadSinkPick> {
    let ds5_card = |id: u32| cards.iter().any(|c| c.id == id && c.ds5);
    // Card nodes only (`device.id`). Split sinks often have no USB ids; the card does.
    let mine: Vec<&SinkNode> = sinks
        .iter()
        .filter(|s| s.device_id.is_some_and(|id| s.ds5 || ds5_card(id)))
        .collect();
    if mine.is_empty() {
        return None;
    }
    let quad = |s: &&&SinkNode| s.channels == 4;
    // Public quads first. Unpositioned ahead of positioned for determinism; `dont-remix` makes them equivalent.
    if let Some(s) = mine
        .iter()
        .filter(|s| !s.internal)
        .filter(quad)
        .find(|s| is_unpositioned(&s.positions))
    {
        return Some(PadSinkPick::Node(s.name.clone()));
    }
    if let Some(s) = mine.iter().filter(|s| !s.internal).find(quad) {
        return Some(PadSinkPick::Node(s.name.clone()));
    }
    // Hidden parent last: index-exact into AUX0..3 drops speaker-left into dead AUX0.
    if let Some(s) = mine.iter().find(quad) {
        return Some(PadSinkPick::Node(s.name.clone()));
    }
    // Restricted clients may not see `Audio/Sink/Internal`; the public split still names it.
    if let Some(parent) = mine
        .iter()
        .find_map(|s| s.split_parent.clone().filter(|p| !p.is_empty()))
    {
        return Some(PadSinkPick::Node(parent));
    }
    mine.first()
        .and_then(|s| s.device_id)
        .map(PadSinkPick::NeedsProfile)
}

/// Build a [`SinkNode`] from the node's INFO props, not the registry announce subset.
///
/// Linux-only: `DictRef` is `pipewire`. `any(..., test)` would compile this into the Windows
/// `lib test` target, where the crate is missing (E0433, `--all-targets` only).
#[cfg(target_os = "linux")]
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

/// Walk every `Audio/Sink…` node and `Device` on a private mainloop.
///
/// Separate from [`crate::audio::devices`]: that walk publishes name + description only.
/// Two rounds: a registry `global` announce has `media.class` / `node.name` / `device.id`
/// but not `audio.channels` or `audio.position` (those live on the bound node's INFO).
/// Reading the announce yields 0 channels and a needless profile swap.
#[cfg(target_os = "linux")]
fn walk_graph() -> anyhow::Result<(Vec<SinkNode>, Vec<CardDevice>)> {
    use anyhow::Context;
    use pipewire as pw;
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    static PW_INIT: std::sync::Once = std::sync::Once::new();
    PW_INIT.call_once(pw::init);

    let mainloop = pw::main_loop::MainLoopRc::new(None).context("pw MainLoop")?;
    let context = pw::context::ContextRc::new(&mainloop, None).context("pw Context")?;
    let core = context
        .connect_rc(None)
        .context("pw connect (is PipeWire running in this session?)")?;
    let registry = core.get_registry_rc().context("pw registry")?;

    let sinks: Rc<RefCell<Vec<SinkNode>>> = Rc::default();
    let cards: Rc<RefCell<Vec<CardDevice>>> = Rc::default();
    // Proxies and listeners must outlive the callback that created them.
    let bound: Rc<RefCell<Vec<(pw::node::Node, pw::node::NodeListener)>>> = Rc::default();

    let _reg_listener = registry
        .add_listener_local()
        .global({
            let (registry, sinks, cards, bound) = (
                registry.clone(),
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

    // Round 1: globals + bind. Round 2: `info` from those binds. Each parks its sync seq.
    let awaited: Rc<Cell<Option<pw::spa::utils::result::AsyncSeq>>> = Rc::new(Cell::new(None));
    let _round_listener = core
        .add_listener_local()
        .done({
            let (mainloop, awaited) = (mainloop.clone(), awaited.clone());
            move |_, seq| {
                if awaited.get() == Some(seq) {
                    mainloop.quit();
                }
            }
        })
        .register();
    for _ in 0..2 {
        awaited.set(Some(core.sync(0).context("pw sync")?));
        mainloop.run();
    }
    let out = (sinks.borrow().clone(), cards.borrow().clone());
    // Drop bound proxies before the core that owns them.
    bound.borrow_mut().clear();
    Ok(out)
}

#[cfg(any(target_os = "linux", test))]
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct CardProfile {
    pub(crate) index: u32,
    pub(crate) name: String,
    pub(crate) description: String,
    /// `SPA_PARAM_PROFILE_available` ≠ `no`. Selecting `no` leaves the card where it was.
    pub(crate) available: bool,
}

/// Pro Audio first (raw `AUX` on every ALSA card). Else a positioned 4-ch (`surround-40` /
/// `quad` / `direct`) — `stream.dont-remix` holds the coils. Stereo/mono/`HiFi` cannot reach them.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn pick_profile(profiles: &[CardProfile]) -> Option<&CardProfile> {
    let usable = |p: &&CardProfile| p.available;
    profiles
        .iter()
        .filter(usable)
        .find(|p| p.name == "pro-audio")
        .or_else(|| {
            profiles.iter().filter(usable).find(|p| {
                p.name.contains("surround-40") || p.name.contains("quad") || p.name == "direct"
            })
        })
}

/// Profile we moved and owe back. One card — v1 renders one DualSense.
#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug)]
struct ProfileSwap {
    device_id: u32,
    previous: u32,
}

#[cfg(target_os = "linux")]
static PROFILE_SWAP: Mutex<Option<ProfileSwap>> = Mutex::new(None);

/// Cards already moved with no four-channel node. Retrying would flip the user's sound settings all session.
#[cfg(target_os = "linux")]
static PROFILE_TRIED: Mutex<Vec<u32>> = Mutex::new(Vec::new());

#[cfg(target_os = "linux")]
enum ProfileTarget {
    FourChannel,
    Index(u32),
}

/// Move the pad's card to a four-channel profile and remember the previous index.
///
/// User-visible, reverted at session end, `save = false` so WirePlumber does not remember it.
/// `PUNKTFUNK_PAD_AUDIO_PROFILE=0` leaves the card alone.
#[cfg(target_os = "linux")]
fn ensure_pro_audio(device_id: u32) -> anyhow::Result<()> {
    if matches!(
        std::env::var("PUNKTFUNK_PAD_AUDIO_PROFILE").as_deref(),
        Ok("0" | "false" | "off" | "no")
    ) {
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

#[cfg(target_os = "linux")]
fn restore_profile() {
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

/// Select a profile, returning the index it had. Three mainloop rounds: registry bind, then
/// `enum_params`, then `set_param` — each waits on the previous replies.
#[cfg(target_os = "linux")]
fn set_card_profile(device_id: u32, want: ProfileTarget) -> anyhow::Result<u32> {
    use anyhow::{anyhow, Context};
    use pipewire as pw;
    use pw::spa::param::ParamType;
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    static PW_INIT: std::sync::Once = std::sync::Once::new();
    PW_INIT.call_once(pw::init);

    let mainloop = pw::main_loop::MainLoopRc::new(None).context("pw MainLoop")?;
    let context = pw::context::ContextRc::new(&mainloop, None).context("pw Context")?;
    let core = context
        .connect_rc(None)
        .context("pw connect (is PipeWire running in this session?)")?;
    let registry = core.get_registry_rc().context("pw registry")?;

    let profiles: Rc<RefCell<Vec<CardProfile>>> = Rc::default();
    let active: Rc<Cell<Option<u32>>> = Rc::new(Cell::new(None));
    let device: Rc<RefCell<Option<pw::device::Device>>> = Rc::default();
    let dev_listener: Rc<RefCell<Option<pw::device::DeviceListener>>> = Rc::default();

    let _reg_listener = registry
        .add_listener_local()
        .global({
            let (registry, device, dev_listener) =
                (registry.clone(), device.clone(), dev_listener.clone());
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

    // One `done` listener for every round; each parks its sync seq here first.
    let awaited: Rc<Cell<Option<pw::spa::utils::result::AsyncSeq>>> = Rc::new(Cell::new(None));
    let _core_listener = core
        .add_listener_local()
        .done({
            let (mainloop, awaited) = (mainloop.clone(), awaited.clone());
            move |_, seq| {
                if awaited.get() == Some(seq) {
                    mainloop.quit();
                }
            }
        })
        .register();
    let round = |issue: &dyn Fn() -> anyhow::Result<()>| -> anyhow::Result<()> {
        issue()?;
        awaited.set(Some(core.sync(0).context("pw sync")?));
        mainloop.run();
        Ok(())
    };

    round(&|| Ok(()))?; // registry replays globals; the card is bound
    round(&|| {
        let d = device.borrow();
        let d = d
            .as_ref()
            .ok_or_else(|| anyhow!("card {device_id} is not in the PipeWire graph"))?;
        d.enum_params(0, Some(ParamType::EnumProfile), 0, u32::MAX);
        d.enum_params(1, Some(ParamType::Profile), 0, 1);
        Ok(())
    })?; // EnumProfile + active Profile

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
    round(&|| {
        let d = device.borrow();
        let d = d
            .as_ref()
            .ok_or_else(|| anyhow!("card {device_id} vanished mid-swap"))?;
        d.set_param(
            ParamType::Profile,
            0,
            pw::spa::pod::Pod::from_bytes(&pod).ok_or_else(|| anyhow!("bad Profile pod"))?,
        );
        Ok(())
    })?; // flush set_param before the loop and its proxies drop
    Ok(previous)
}

#[cfg(target_os = "linux")]
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
#[cfg(target_os = "linux")]
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
#[cfg(target_os = "linux")]
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
#[cfg(target_os = "linux")]
fn pin_sink_volume(node_id: u32, channels: u32) -> anyhow::Result<()> {
    use anyhow::{anyhow, Context};
    use pipewire as pw;
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    static PW_INIT: std::sync::Once = std::sync::Once::new();
    PW_INIT.call_once(pw::init);

    let mainloop = pw::main_loop::MainLoopRc::new(None).context("pw MainLoop")?;
    let context = pw::context::ContextRc::new(&mainloop, None).context("pw Context")?;
    let core = context.connect_rc(None).context("pw connect")?;
    let registry = core.get_registry_rc().context("pw registry")?;

    let node: Rc<RefCell<Option<pw::node::Node>>> = Rc::default();
    let _reg_listener = registry
        .add_listener_local()
        .global({
            let (registry, node) = (registry.clone(), node.clone());
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

    let awaited: Rc<Cell<Option<pw::spa::utils::result::AsyncSeq>>> = Rc::new(Cell::new(None));
    let _core_listener = core
        .add_listener_local()
        .done({
            let (mainloop, awaited) = (mainloop.clone(), awaited.clone());
            move |_, seq| {
                if awaited.get() == Some(seq) {
                    mainloop.quit();
                }
            }
        })
        .register();
    let round = |issue: &dyn Fn() -> anyhow::Result<()>| -> anyhow::Result<()> {
        issue()?;
        awaited.set(Some(core.sync(0).context("pw sync")?));
        mainloop.run();
        Ok(())
    };

    round(&|| Ok(()))?; // registry replays globals; the node is bound
    let pod = unity_volume_pod(channels).context("serialize Props pod")?;
    round(&|| {
        let n = node.borrow();
        let n = n
            .as_ref()
            .ok_or_else(|| anyhow!("sink node {node_id} is not in the PipeWire graph"))?;
        n.set_param(
            pw::spa::param::ParamType::Props,
            0,
            pw::spa::pod::Pod::from_bytes(&pod).ok_or_else(|| anyhow!("bad Props pod"))?,
        );
        Ok(())
    })?; // flush set_param before the loop and its proxies drop
    Ok(())
}

/// Pin the picked node to unity on every (re)correlation — a profile change remints nodes.
#[cfg(target_os = "linux")]
fn pin_picked(name: String, sinks: &[SinkNode]) -> String {
    if matches!(
        std::env::var("PUNKTFUNK_PAD_SINK_VOLUME").as_deref(),
        Ok("0" | "false" | "off" | "no")
    ) {
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

#[cfg(target_os = "linux")]
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
#[cfg(target_os = "linux")]
pub fn pad_audio_test(seconds: u64, coils: bool, speaker: bool) -> anyhow::Result<()> {
    // Restore even on `?` — otherwise a failed test leaves the card on Pro Audio.
    let out = pad_audio_test_inner(seconds, coils, speaker);
    restore_profile();
    out
}

#[cfg(target_os = "linux")]
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

#[cfg(any(windows, test))]
pub(crate) struct EndpointCandidate {
    /// `IMMDevice` id (`{0.0.0.00000000}.{…}`) — WASAPI device targeting.
    pub(crate) id: String,
    pub(crate) container: Option<String>,
    pub(crate) channels: u16,
}

/// Container match AND 4-channel format — the DS5 audio function is the only 4-ch endpoint in its container.
#[cfg(any(windows, test))]
pub(crate) fn pick_pad_endpoint<'a>(
    endpoints: &'a [EndpointCandidate],
    container: &str,
) -> Option<&'a EndpointCandidate> {
    endpoints.iter().find(|e| {
        e.channels == 4
            && e.container
                .as_deref()
                .is_some_and(|c| c.eq_ignore_ascii_case(container))
    })
}

/// Interface path → instance id: strip `\\?\` / `\\.`, `#` → `\`, drop trailing `{guid}`.
/// That id is the Enum key where `ContainerID` lives.
#[cfg(any(windows, test))]
pub(crate) fn hid_instance_from_interface_path(path: &str) -> Option<String> {
    let p = path
        .strip_prefix(r"\\?\")
        .or_else(|| path.strip_prefix(r"\\.\"))
        .unwrap_or(path);
    let mut segs: Vec<&str> = p.split('#').collect();
    if let Some(last) = segs.last() {
        if last.starts_with('{') && last.ends_with('}') {
            segs.pop();
        }
    }
    if segs.len() != 3 || segs.iter().any(|s| s.is_empty()) {
        return None;
    }
    Some(segs.join("\\"))
}

/// `VT_CLSID` PROPVARIANT blob: 8-byte header `[vt,0,0,0,1,0,0,0]` then registry-order GUID.
#[cfg(any(windows, test))]
pub(crate) fn container_guid_from_blob(bytes: &[u8]) -> Option<String> {
    const VT_CLSID: u8 = 0x48;
    if bytes.len() < 24 || bytes[0] != VT_CLSID {
        return None;
    }
    let g = &bytes[8..24];
    let d1 = u32::from_le_bytes([g[0], g[1], g[2], g[3]]);
    let d2 = u16::from_le_bytes([g[4], g[5]]);
    let d3 = u16::from_le_bytes([g[6], g[7]]);
    Some(format!(
        "{{{:08x}-{:04x}-{:04x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}}}",
        d1, d2, d3, g[8], g[9], g[10], g[11], g[12], g[13], g[14], g[15]
    ))
}

/// HID interface path → instance id → Enum `ContainerID` → 4-ch eRender endpoint with matching
/// `PKEY_Device_ContainerId`. Registry for property reads (MMDevices ACL denies writes, not reads).
#[cfg(windows)]
pub(crate) fn correlate_pad_endpoint(hid_path: &str) -> anyhow::Result<String> {
    use anyhow::Context;
    let instance = hid_instance_from_interface_path(hid_path)
        .with_context(|| format!("unrecognised HID interface path shape: {hid_path}"))?;
    let container = hid_container_id(&instance)
        .with_context(|| format!("no ContainerID on devnode {instance}"))?;
    let endpoints = render_endpoints()?;
    pick_pad_endpoint(&endpoints, &container)
        .map(|e| e.id.clone())
        .with_context(|| {
            format!(
                "no active 4-ch render endpoint in container {container} \
                 ({} endpoints inspected)",
                endpoints.len()
            )
        })
}

#[cfg(windows)]
fn hid_container_id(instance: &str) -> anyhow::Result<String> {
    use anyhow::Context;
    let key = winreg::RegKey::predef(winreg::enums::HKEY_LOCAL_MACHINE)
        .open_subkey(format!(r"SYSTEM\CurrentControlSet\Enum\{instance}"))
        .with_context(|| format!(r"open Enum\{instance}"))?;
    key.get_value::<String, _>("ContainerID")
        .context("read ContainerID")
}

/// Active eRender endpoints. Own MTA thread (caller may be STA); one broken endpoint must not hide the rest.
#[cfg(windows)]
fn render_endpoints() -> anyhow::Result<Vec<EndpointCandidate>> {
    use anyhow::{anyhow, Context};
    std::thread::Builder::new()
        .name("pf-pad-audio-enum".into())
        .spawn(|| -> anyhow::Result<Vec<EndpointCandidate>> {
            wasapi::initialize_mta()
                .ok()
                .context("CoInitializeEx (MTA)")?;
            let enumerator = wasapi::DeviceEnumerator::new().context("DeviceEnumerator")?;
            let coll = enumerator
                .get_device_collection(&wasapi::Direction::Render)
                .context("render endpoint collection")?;
            let mut out = Vec::new();
            for i in 0..coll.get_nbr_devices().context("endpoint count")? {
                let Ok(dev) = coll.get_device_at_index(i) else {
                    continue;
                };
                let Ok(id) = dev.get_id() else {
                    continue;
                };
                let channels = dev
                    .get_device_format()
                    .map(|f| f.get_nchannels())
                    .unwrap_or(0);
                out.push(EndpointCandidate {
                    container: endpoint_container_id(&id),
                    id,
                    channels,
                });
            }
            Ok(out)
        })
        .context("spawn pad-audio enumeration thread")?
        .join()
        .map_err(|_| anyhow!("pad-audio enumeration thread panicked"))?
}

/// `PKEY_Device_ContainerId` from the MMDevices property store (`…\Render\{ep}\Properties`, VT_CLSID blob).
#[cfg(windows)]
fn endpoint_container_id(endpoint_id: &str) -> Option<String> {
    let guid = endpoint_id.rfind('{').map(|i| &endpoint_id[i..])?;
    let key = winreg::RegKey::predef(winreg::enums::HKEY_LOCAL_MACHINE)
        .open_subkey(format!(
            r"SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\Render\{guid}\Properties"
        ))
        .ok()?;
    let v = key
        .get_raw_value("{8c7ed206-3f8a-4827-b3ab-ae9e1faefc6c},2")
        .ok()?;
    container_guid_from_blob(&v.bytes)
}

/// `frame_samples` is the PLC synthesis unit (session audio-thread discipline).
struct KindStream {
    dec: opus::Decoder,
    gaps: AudioGapTracker,
    frame_samples: usize,
}

/// Pad-audio renderer: 0xD1 consumer. Opens the device on the first frame so a session without
/// a wired DualSense is an idle 10 ms poll. Exits on the session stop flag or the plane closing.
pub(crate) fn spawn(
    connector: Arc<NativeClient>,
    stop: Arc<AtomicBool>,
    haptics: bool,
    speaker: bool,
) -> Option<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("pf-pad-audio".into())
        .spawn(move || run(&connector, &stop, haptics, speaker))
        .map_err(|e| tracing::warn!(error = %e, "pad-audio thread start failed"))
        .ok()
}

fn run(connector: &NativeClient, stop: &AtomicBool, haptics: bool, speaker: bool) {
    // Late decode is rumble after the hit. Same best-effort RT as the main decode leg.
    crate::audio_rt::boost_and_log("pf-pad-audio");
    // v1: first streaming pad; per-(pad, kind) degenerates to per-kind once latched.
    let mut streams: [Option<KindStream>; 2] = [None, None];
    let mut mixer = QuadMixer::<f32>::new(MAX_BUFFER_FRAMES);
    let mut pcm = vec![0f32; 5760 * 2]; // max Opus frame (120 ms) × stereo
    let mut out: Option<PadOut> = None;
    let mut active_pad: Option<u8> = None;
    let mut other_pad_logged = false;
    let mut open_fail_logged = false;
    let mut retry_at = Instant::now();
    let mut backoff = RETRY_MIN;
    while !stop.load(Ordering::SeqCst) {
        let Some(f) = connector.next_pad_audio(Duration::from_millis(10)) else {
            if connector.is_session_ended() {
                break;
            }
            continue;
        };
        // Host only emits declared kinds; re-check settings so a stale host cannot force a renderer.
        if f.kind > 1
            || (f.kind == PAD_AUDIO_KIND_HAPTICS && !haptics)
            || (f.kind == PAD_AUDIO_KIND_SPEAKER && !speaker)
        {
            continue;
        }
        // v1: one DualSense. Latch the first streaming pad, drop the rest.
        match active_pad {
            None => active_pad = Some(f.pad),
            Some(p) if p != f.pad => {
                if !other_pad_logged {
                    other_pad_logged = true;
                    tracing::info!(
                        rendered = p,
                        ignored = f.pad,
                        "pad audio from a second pad — v1 renders one physical DualSense"
                    );
                }
                continue;
            }
            _ => {}
        }
        // Rendered haptics take the coils from wire rumble (`haptics_live`); concealment is
        // not evidence, and nothing renders without an output.
        if f.kind == PAD_AUDIO_KIND_HAPTICS && !f.opus.is_empty() && out.is_some() {
            HAPTICS.note(f.pad);
        }
        let k = f.kind as usize;
        if streams[k].is_none() {
            match opus::Decoder::new(48_000, opus::Channels::Stereo) {
                Ok(dec) => {
                    streams[k] = Some(KindStream {
                        dec,
                        gaps: AudioGapTracker::new(),
                        frame_samples: 0,
                    })
                }
                Err(e) => {
                    tracing::warn!(error = %e, kind = f.kind, "pad-audio opus decoder failed");
                    continue;
                }
            }
        }
        let st = streams[k].as_mut().expect("inserted above");
        // Seq-gap PLC before decode. A frozen seq (host paused) produces no packets — silence.
        for _ in 0..plc_frames(&mut st.gaps, f.seq, st.frame_samples) {
            let n = st.frame_samples * 2;
            if let Ok(samples) = st.dec.decode_float(&[], &mut pcm[..n], false) {
                mixer.push(f.kind, &pcm[..samples * 2], Instant::now());
            }
        }
        if !f.opus.is_empty() {
            match st.dec.decode_float(&f.opus, &mut pcm, false) {
                Ok(samples) => {
                    st.frame_samples = samples;
                    mixer.push(f.kind, &pcm[..samples * 2], Instant::now());
                }
                Err(e) => tracing::debug!(error = %e, kind = f.kind, "pad-audio opus decode"),
            }
        }
        // Open lazily; drop + re-correlate with backoff when the USB sink/endpoint vanishes.
        if out.as_ref().is_some_and(PadOut::finished) {
            tracing::info!("pad-audio output ended (device gone?) — re-correlating");
            out = None;
            retry_at = Instant::now() + backoff;
            backoff = (backoff * 2).min(RETRY_MAX);
        }
        if out.is_none() && Instant::now() >= retry_at {
            match PadOut::open() {
                Ok(o) => {
                    tracing::info!("pad-audio output opened on the DualSense audio device");
                    out = Some(o);
                    backoff = RETRY_MIN;
                    open_fail_logged = false;
                }
                Err(e) => {
                    if !open_fail_logged {
                        open_fail_logged = true;
                        tracing::warn!(
                            error = %format!("{e:#}"),
                            "no DualSense audio device — pad audio parked (retrying with backoff)"
                        );
                    }
                    retry_at = Instant::now() + backoff;
                    backoff = (backoff * 2).min(RETRY_MAX);
                }
            }
        }
        match &out {
            Some(o) => {
                let mut chunk = o.take_buffer();
                if mixer.pop(&mut chunk, Instant::now()) > 0 {
                    o.push(chunk);
                }
            }
            None => mixer.discard(),
        }
    }
    // Drop output before restoring the profile: the card cannot leave a profile whose PCM is still open.
    drop(out);
    #[cfg(target_os = "linux")]
    restore_profile();
    tracing::debug!("pad-audio pull thread exited");
}

/// `finished()` is device-gone — the worker drops and re-correlates.
#[cfg(target_os = "linux")]
struct PadOut {
    pcm_tx: std::sync::mpsc::SyncSender<Vec<f32>>,
    recycle_rx: std::sync::mpsc::Receiver<Vec<f32>>,
    quit_tx: pipewire::channel::Sender<()>,
    thread: Option<std::thread::JoinHandle<()>>,
}

#[cfg(target_os = "linux")]
impl PadOut {
    fn open() -> anyhow::Result<PadOut> {
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

    fn take_buffer(&self) -> Vec<f32> {
        self.recycle_rx.try_recv().unwrap_or_default()
    }

    fn push(&self, pcm: Vec<f32>) {
        let _ = self.pcm_tx.try_send(pcm); // never block the renderer; drops are concealed
    }

    fn finished(&self) -> bool {
        self.thread.as_ref().is_none_or(|t| t.is_finished())
    }
}

#[cfg(target_os = "linux")]
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
#[cfg(target_os = "linux")]
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

    static PW_INIT: std::sync::Once = std::sync::Once::new();
    PW_INIT.call_once(pw::init);

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

#[cfg(windows)]
struct PadOut {
    pcm_tx: std::sync::mpsc::SyncSender<Vec<f32>>,
    recycle_rx: std::sync::mpsc::Receiver<Vec<f32>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

#[cfg(windows)]
impl PadOut {
    /// Shared event-driven render stream (`audio_wasapi::render_thread` shape).
    fn open() -> anyhow::Result<PadOut> {
        use anyhow::{anyhow, Context};
        let hid_path =
            first_tier_a_hid_path().ok_or_else(|| anyhow!("no tier-A pad registered"))?;
        let endpoint = correlate_pad_endpoint(&hid_path)?;
        tracing::info!(endpoint = %endpoint, "pad-audio endpoint correlated");
        let (pcm_tx, pcm_rx) = std::sync::mpsc::sync_channel::<Vec<f32>>(64);
        let (recycle_tx, recycle_rx) = std::sync::mpsc::sync_channel::<Vec<f32>>(64);
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<anyhow::Result<()>>(1);
        let stop = Arc::new(AtomicBool::new(false));
        let stop_t = stop.clone();
        let thread = std::thread::Builder::new()
            .name("pf-pad-audio-out".into())
            .spawn(move || {
                if let Err(e) = pad_render_thread(pcm_rx, recycle_tx, stop_t, ready_tx, &endpoint) {
                    tracing::warn!(error = %format!("{e:#}"), "pad-audio render thread ended");
                }
            })
            .context("spawn pad-audio render thread")?;
        match ready_rx.recv_timeout(Duration::from_secs(3)) {
            Ok(Ok(())) => Ok(PadOut {
                pcm_tx,
                recycle_rx,
                stop,
                thread: Some(thread),
            }),
            Ok(Err(e)) => Err(e),
            Err(_) => {
                // A late open must not leave a thread holding the pad endpoint.
                stop.store(true, Ordering::SeqCst);
                Err(anyhow!("pad-audio render init timed out"))
            }
        }
    }

    fn take_buffer(&self) -> Vec<f32> {
        self.recycle_rx.try_recv().unwrap_or_default()
    }

    fn push(&self, pcm: Vec<f32>) {
        let _ = self.pcm_tx.try_send(pcm); // never block the renderer; drops are concealed
    }

    fn finished(&self) -> bool {
        self.thread.as_ref().is_none_or(|t| t.is_finished())
    }
}

#[cfg(windows)]
impl Drop for PadOut {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Shared event-driven WASAPI on the correlated endpoint. 4 ch f32 masked FL|FR|BL|BR (0x33 —
/// the DS5 layout, identity map). Ring floor [240, 2400] frames. Any device error ends the thread.
#[cfg(windows)]
fn pad_render_thread(
    pcm_rx: std::sync::mpsc::Receiver<Vec<f32>>,
    recycle_tx: std::sync::mpsc::SyncSender<Vec<f32>>,
    stop: Arc<AtomicBool>,
    ready: std::sync::mpsc::SyncSender<anyhow::Result<()>>,
    endpoint_id: &str,
) -> anyhow::Result<()> {
    use anyhow::{anyhow, Context};
    use wasapi::{Direction, SampleType, StreamMode, WaveFormat};
    if let Err(e) = wasapi::initialize_mta()
        .ok()
        .context("CoInitializeEx (MTA)")
    {
        let _ = ready.send(Err(e));
        return Ok(());
    }
    let res = (|| -> anyhow::Result<()> {
        const BLOCK_ALIGN: usize = PAD_CHANNELS * 4; // f32 interleaved
        let enumerator = wasapi::DeviceEnumerator::new().context("DeviceEnumerator")?;
        // Not `get_device`: wasapi 0.23 resolved through a freed string. Active-only filter:
        // [`crate::audio::device_by_id`] (`audio_wasapi.rs` via lib.rs `#[path]`).
        let device = crate::audio::device_by_id(&enumerator, &Direction::Render, endpoint_id)
            .map_err(|e| anyhow!("correlated endpoint not found: {e:#}"))?;
        let mut audio_client = device.get_iaudioclient().context("IAudioClient")?;
        // FL|FR|BL|BR: front = speaker, back = voice coils.
        let desired = WaveFormat::new(32, 32, &SampleType::Float, 48_000, PAD_CHANNELS, Some(0x33));
        let (default_period, _min_period) =
            audio_client.get_device_period().context("device period")?;
        let mode = StreamMode::EventsShared {
            autoconvert: true,
            buffer_duration_hns: default_period,
        };
        audio_client
            .initialize_client(&desired, &Direction::Render, &mode)
            .context("initialize pad render client")?;
        let h_event = audio_client.set_get_eventhandle().context("event handle")?;
        let render_client = audio_client
            .get_audiorenderclient()
            .context("IAudioRenderClient")?;
        audio_client
            .start_stream()
            .context("start pad render stream")?;
        let _ = ready.send(Ok(()));

        // Adaptive jitter buffer in f32-byte units, pad floor (see [`pad_render_thread`]).
        let mut ring: std::collections::VecDeque<u8> = std::collections::VecDeque::new();
        let mut primed = false;
        let mut out = Vec::new();
        let mut silent_waits: u32 = 0;

        while !stop.load(Ordering::Relaxed) {
            if h_event.wait_for_event(100).is_err() {
                // An unplugged pad stops signalling without an error: end, and the worker
                // re-correlates, as the main render path reopens.
                silent_waits += 1;
                if silent_waits >= crate::audio_wasapi::EVENT_SILENT_WAITS {
                    return Err(anyhow!("the pad render event stopped"));
                }
                continue;
            }
            silent_waits = 0;
            while let Ok(mut chunk) = pcm_rx.try_recv() {
                for s in chunk.iter() {
                    ring.extend(s.to_le_bytes());
                }
                chunk.clear();
                let _ = recycle_tx.try_send(chunk);
            }
            let avail_frames = audio_client
                .get_available_space_in_frames()
                .context("available space")? as usize;
            if avail_frames == 0 {
                continue;
            }
            let want_bytes = avail_frames * BLOCK_ALIGN;

            // Prime ~3 quanta in [240, 2400] frames; cap ~1 quantum of slack; re-prime on a drain.
            let target = (3 * want_bytes).clamp(240 * BLOCK_ALIGN, 2400 * BLOCK_ALIGN);
            let cap = target.max(want_bytes) + want_bytes;
            if ring.len() > cap {
                ring.drain(..ring.len() - cap);
            }
            if !primed && ring.len() >= target {
                primed = true;
            }

            out.clear();
            out.resize(want_bytes, 0);
            if primed {
                let n = ring.len().min(want_bytes);
                for (dst, b) in out.iter_mut().zip(ring.drain(..n)) {
                    *dst = b;
                }
            }
            if ring.is_empty() {
                primed = false;
            }
            render_client
                .write_to_device(avail_frames, &out, None)
                .context("write_to_device")?;
        }
        audio_client.stop_stream().ok();
        Ok(())
    })();
    if let Err(ref e) = res {
        let _ = ready.send(Err(anyhow::anyhow!("{e:#}")));
    }
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `channelVolumes` length must match the port count; a mismatch is ignored and looks like the pin silently failed.
    #[cfg(target_os = "linux")]
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

    #[test]
    fn speaker_mode_gates() {
        assert!(speaker_active("pad"));
        assert!(!speaker_active("off"));
        assert!(!speaker_active("mix"));
        assert!(!speaker_active(""));
        assert!(!speaker_active("Pad")); // stored names are lowercase
    }

    #[test]
    fn tier_a_is_wired_ds5_or_edge_only() {
        assert!(is_tier_a_ds5(0x054C, 0x0CE6, true));
        assert!(is_tier_a_ds5(0x054C, 0x0DF2, true));
        assert!(!is_tier_a_ds5(0x054C, 0x0CE6, false));
        assert!(!is_tier_a_ds5(0x054C, 0x05C4, true));
        assert!(!is_tier_a_ds5(0x045E, 0x0CE6, true));
        assert!(!is_tier_a_ds5(0x28DE, 0x1205, true));
    }

    #[test]
    fn ds5_sink_signature_matching() {
        assert!(is_ds5_sink(
            "alsa_output.usb-Sony_Interactive_Entertainment_Wireless_Controller-00.analog-stereo",
            "Wireless Controller Analog Stereo"
        ));
        assert!(is_ds5_sink(
            "alsa_output.usb-054c_0ce6-00",
            "DualSense Wireless Controller"
        ));
        assert!(is_ds5_sink("Wireless Controller", ""));
        assert!(is_ds5_sink("", "Wireless Controller Audio"));
        assert!(!is_ds5_sink(
            "alsa_output.pci-0000_0a_00.4.analog-stereo",
            "Built-in Audio Analog Stereo"
        ));
        // `starts_with("Wireless Controller")` — a headset "… for Wireless Controller" is not the pad.
        assert!(!is_ds5_sink("headset", "Adapter for Wireless Controller"));
    }

    #[test]
    fn usb_ids_are_hex_either_spelling() {
        assert_eq!(parse_usb_id("054c"), Some(0x054C));
        assert_eq!(parse_usb_id("0x054c"), Some(0x054C));
        assert_eq!(parse_usb_id("0X0CE6"), Some(0x0CE6));
        assert_eq!(parse_usb_id(" 0df2 "), Some(0x0DF2));
        assert_eq!(parse_usb_id("0994"), Some(0x0994)); // hex, not decimal 994
        assert_eq!(parse_usb_id(""), None);
        assert_eq!(parse_usb_id("Sony"), None);
    }

    #[test]
    fn ds5_identity_from_ids_or_name() {
        assert!(props_say_ds5(
            Some("054c"),
            Some("0ce6"),
            "alsa_card.usb-x",
            ""
        ));
        assert!(props_say_ds5(Some("0x054C"), Some("0x0DF2"), "", ""));
        assert!(!props_say_ds5(Some("054c"), Some("0104"), "", "")); // other Sony audio
        assert!(!props_say_ds5(Some("046d"), Some("0ce6"), "", ""));
        assert!(!props_say_ds5(None, None, "alsa_card.pci-0000_0a_00.4", ""));
        // Split UCM sinks publish no ids; the name carries identity.
        assert!(props_say_ds5(
            None,
            None,
            "alsa_output.usb-Sony_Interactive_Entertainment_Wireless_Controller-00.HiFi__Speaker__sink",
            "Speaker"
        ));
    }

    #[test]
    fn aux_and_unknown_maps_are_unpositioned() {
        let v = |s: &str| -> Vec<String> { s.split(',').map(|p| p.trim().to_string()).collect() };
        assert!(is_unpositioned(&v("AUX0,AUX1,AUX2,AUX3")));
        assert!(is_unpositioned(&v("UNK,UNK,UNK,UNK")));
        assert!(is_unpositioned(&[]));
        assert!(!is_unpositioned(&v("FL,FR,RL,RR")));
        assert!(!is_unpositioned(&v("MONO")));
        assert!(!is_unpositioned(&v("AUX0,AUX1,FL,FR")));
    }

    fn sink(name: &str, channels: u32, positions: &str, device_id: Option<u32>) -> SinkNode {
        SinkNode {
            // Picker never reads `id` (only the volume pin does).
            id: 0,
            name: name.into(),
            description: String::new(),
            device_id,
            channels,
            positions: if positions.is_empty() {
                Vec::new()
            } else {
                positions.split(',').map(str::to_string).collect()
            },
            split_parent: None,
            ds5: true,
            internal: false,
        }
    }

    #[test]
    fn pad_sink_pick_needs_four_channels_on_a_card() {
        let cards = [CardDevice {
            id: 42,
            ds5: true,
            ..CardDevice::default()
        }];
        let sinks = [
            sink("ds5.analog-stereo", 2, "FL,FR", Some(42)),
            sink("ds5.analog-surround-40", 4, "FL,FR,RL,RR", Some(42)),
            sink("ds5.pro-output-0", 4, "AUX0,AUX1,AUX2,AUX3", Some(42)),
        ];
        assert_eq!(
            pick_pad_sink(&sinks, &cards),
            Some(PadSinkPick::Node("ds5.pro-output-0".into()))
        );
        // Positioned quad is equivalent under `dont-remix`.
        assert_eq!(
            pick_pad_sink(&sinks[..2], &cards),
            Some(PadSinkPick::Node("ds5.analog-surround-40".into()))
        );
        assert_eq!(
            pick_pad_sink(&sinks[..1], &cards),
            Some(PadSinkPick::NeedsProfile(42))
        );
        assert_eq!(pick_pad_sink(&[], &cards), None);
    }

    /// Host-minted pad sinks carry DualSense identity on purpose and have no `device.id`.
    /// Rendering into one loops the plane at the host.
    #[test]
    fn pad_sink_pick_skips_a_virtual_host_sink() {
        let virtual_sink = sink(
            "alsa_output.usb-Sony_Interactive_Entertainment_Wireless_Controller-00.HiFi__Speaker__sink",
            4,
            "AUX0,AUX1,AUX2,AUX3",
            None,
        );
        assert_eq!(
            pick_pad_sink(std::slice::from_ref(&virtual_sink), &[]),
            None
        );
        let cards = [CardDevice {
            id: 7,
            ds5: true,
            ..CardDevice::default()
        }];
        let real = sink("ds5.pro-output-0", 4, "AUX0,AUX1,AUX2,AUX3", Some(7));
        assert_eq!(
            pick_pad_sink(&[virtual_sink, real], &cards),
            Some(PadSinkPick::Node("ds5.pro-output-0".into()))
        );
    }

    /// UCM split card: public 4-ch `SpeakerHaptic`, mono `Speaker`, hidden 4-ch parent.
    /// Public 4-ch must win from any registry order; the parent is fallback (AUX0 is dead).
    #[test]
    fn pad_sink_pick_on_a_real_steamos_dualsense() {
        let cards = [CardDevice {
            id: 140,
            name: "alsa_card.usb-Sony_Interactive_Entertainment_DualSense_Wireless_Controller-00"
                .into(),
            description: "DualSense wireless controller (PS5)".into(),
            ds5: true,
        }];
        let base =
            "alsa_output.usb-Sony_Interactive_Entertainment_DualSense_Wireless_Controller-00";
        let mut parent = sink(
            "alsa_output.hw_Controller_0",
            4,
            "AUX0,AUX1,AUX2,AUX3",
            Some(140),
        );
        parent.internal = true;
        parent.ds5 = false; // parent proplist has no vendor ids or product name
        let mut haptic = sink(
            &format!("{base}.HiFi__SpeakerHaptic__sink"),
            4,
            "FL,FR,RL,RR",
            Some(140),
        );
        haptic.split_parent = Some("alsa_output.hw_Controller_0".into());
        let mut mono = sink(&format!("{base}.HiFi__Speaker__sink"), 1, "MONO", Some(140));
        mono.split_parent = Some("alsa_output.hw_Controller_0".into());

        // Registry order is not ours; the public 4-ch sink must win from any of them.
        for order in [
            vec![parent.clone(), haptic.clone(), mono.clone()],
            vec![mono.clone(), parent.clone(), haptic.clone()],
            vec![haptic.clone(), mono.clone(), parent.clone()],
        ] {
            assert_eq!(
                pick_pad_sink(&order, &cards),
                Some(PadSinkPick::Node(format!(
                    "{base}.HiFi__SpeakerHaptic__sink"
                ))),
                "the four-channel public sink must win from any enumeration order"
            );
        }
        // Mono + parent: parent is the right fallback (coils over a full speaker).
        assert_eq!(
            pick_pad_sink(&[mono, parent], &cards),
            Some(PadSinkPick::Node("alsa_output.hw_Controller_0".into()))
        );
    }

    /// Split public sinks are mono/stereo; the named four-channel parent holds the coils.
    /// Identity is on the card, not the split sinks.
    #[test]
    fn pad_sink_pick_follows_a_split_parent() {
        let cards = [CardDevice {
            id: 3,
            ds5: true,
            ..CardDevice::default()
        }];
        let mut speaker = sink("ds5.HiFi__Speaker__sink", 1, "MONO", Some(3));
        speaker.ds5 = false; // identity is on the card
        speaker.split_parent = Some("alsa_output.hw_3_0".into());
        let mut phones = sink("ds5.HiFi__Headphones__sink", 2, "FL,FR", Some(3));
        phones.ds5 = false;
        assert_eq!(
            pick_pad_sink(&[speaker, phones], &cards),
            Some(PadSinkPick::Node("alsa_output.hw_3_0".into()))
        );
    }

    #[test]
    fn profile_choice_prefers_pro_audio() {
        let p = |name: &str, index: u32, available: bool| CardProfile {
            index,
            name: name.into(),
            description: name.into(),
            available,
        };
        let all = [
            p("off", 0, true),
            p("output:analog-stereo", 1, true),
            p("output:analog-surround-40", 2, true),
            p("pro-audio", 3, true),
        ];
        assert_eq!(pick_profile(&all).map(|p| p.index), Some(3));
        assert_eq!(pick_profile(&all[..3]).map(|p| p.index), Some(2));
        assert_eq!(pick_profile(&all[..2]).map(|p| p.index), None);
        // Unavailable Pro Audio must fall through, not be selected.
        let unavailable = [
            p("output:analog-surround-40", 2, true),
            p("pro-audio", 3, false),
        ];
        assert_eq!(pick_profile(&unavailable).map(|p| p.index), Some(2));
    }

    /// Container match is case-insensitive (Enum uppercase, MMDevices lowercase) and requires 4 channels.
    #[test]
    fn endpoint_pick_needs_container_and_four_channels() {
        let cands = [
            EndpointCandidate {
                id: "{0.0.0.00000000}.{aaaa}".into(),
                container: Some("{11111111-2222-3333-4444-555555555555}".into()),
                channels: 2, // right container, stereo — not the pad
            },
            EndpointCandidate {
                id: "{0.0.0.00000000}.{bbbb}".into(),
                container: Some("{99999999-2222-3333-4444-555555555555}".into()),
                channels: 4, // 4-ch, other container
            },
            EndpointCandidate {
                id: "{0.0.0.00000000}.{cccc}".into(),
                container: Some("{11111111-2222-3333-4444-555555555555}".into()),
                channels: 4,
            },
            EndpointCandidate {
                id: "{0.0.0.00000000}.{dddd}".into(),
                container: None,
                channels: 4,
            },
        ];
        let hit = pick_pad_endpoint(&cands, "{11111111-2222-3333-4444-555555555555}").unwrap();
        assert_eq!(hit.id, "{0.0.0.00000000}.{cccc}");
        let hit = pick_pad_endpoint(
            &cands,
            "{11111111-2222-3333-4444-555555555555}"
                .to_uppercase()
                .as_str(),
        )
        .unwrap();
        assert_eq!(hit.id, "{0.0.0.00000000}.{cccc}");
        assert!(pick_pad_endpoint(&cands, "{00000000-0000-0000-0000-000000000000}").is_none());
    }

    #[test]
    fn hid_interface_path_to_instance_id() {
        assert_eq!(
            hid_instance_from_interface_path(
                r"\\?\HID#VID_054C&PID_0CE6#8&2de99099&0&0000#{4d1e55b2-f16f-11cf-88cb-001111000030}"
            )
            .as_deref(),
            Some(r"HID\VID_054C&PID_0CE6\8&2de99099&0&0000")
        );
        // hidapi: lowercase, sometimes no class GUID.
        assert_eq!(
            hid_instance_from_interface_path(r"\\?\hid#vid_054c&pid_0df2#7&1a2b3c4d&1&0000")
                .as_deref(),
            Some(r"hid\vid_054c&pid_0df2\7&1a2b3c4d&1&0000")
        );
        for bad in ["", "/dev/hidraw3", r"\\?\HID#VID_054C", "a#b#c#d#e"] {
            assert_eq!(
                hid_instance_from_interface_path(bad),
                None,
                "{bad:?} parsed"
            );
        }
    }

    #[test]
    fn container_blob_parses_vt_clsid() {
        // {11223344-5566-7788-99aa-bbccddeeff00}: data1/2/3 little-endian on disk.
        let mut blob = vec![0x48, 0, 0, 0, 1, 0, 0, 0];
        blob.extend_from_slice(&[
            0x44, 0x33, 0x22, 0x11, 0x66, 0x55, 0x88, 0x77, 0x99, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE,
            0xFF, 0x00,
        ]);
        assert_eq!(
            container_guid_from_blob(&blob).as_deref(),
            Some("{11223344-5566-7788-99aa-bbccddeeff00}")
        );
        assert_eq!(container_guid_from_blob(&blob[..20]), None);
        let mut wrong_vt = blob.clone();
        wrong_vt[0] = 0x41; // VT_BLOB, not a container
        assert_eq!(container_guid_from_blob(&wrong_vt), None);
    }
}
