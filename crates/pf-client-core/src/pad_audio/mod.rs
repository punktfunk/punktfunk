//! DualSense haptics and speaker (`0xD1`) on the pad in the player's hands: a wired pad's
//! four-channel USB audio device, or a Bluetooth pad's HID link ([`bluetooth`]).
//!
//! [`spawn`] decodes both Opus streams with gap concealment and interleaves speaker on 0/1
//! and voice coils on 2/3. A wired pad plays on its WASAPI or PipeWire endpoint. Linux
//! needs the card's four-channel profile, index-based `AUX0..AUX3` mapping, and exclusion
//! of host-minted look-alike sinks; `ensure_pro_audio` moves the profile for the session
//! and restores it on exit.
//!
//! `pad_haptics` and `pad_speaker` gate capability advertisement. Speaker `"mix"`
//! is not implemented and behaves as `"off"`.

mod bluetooth;
// The wired pad's USB audio device. One cfg picks the backend; both expose the same `PadOut`.
#[cfg_attr(target_os = "linux", path = "pipewire.rs")]
#[cfg_attr(windows, path = "wasapi.rs")]
mod usb;

use punktfunk_core::audio::pad_mix::{
    is_haptics_evidence, HapticsLiveness, PadDecode, QuadMixer, MAX_FRAME_SAMPLES,
};
use punktfunk_core::client::NativeClient;
use punktfunk_core::input::MAX_PADS;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
pub(crate) use usb::wired_audio_sibling;
use usb::PadOut;
#[cfg(target_os = "linux")]
pub use usb::{correlate_pad_sink, pad_audio_test};

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

/// DualSense / DualSense Edge: wired through the 4-ch USB audio device, wireless through the
/// HID link.
pub(crate) fn is_tier_a_ds5(vid: u16, pid: u16) -> bool {
    vid == 0x054C && matches!(pid, 0x0CE6 | 0x0DF2)
}

struct TierAPad {
    index: u8,
    /// Windows correlation and the Bluetooth sink; Linux matches a USB sink by signature.
    hid_path: Option<String>,
    bluetooth: bool,
}

/// Shared by the app-lifetime gamepad worker (write at slot open/close) and the per-session
/// renderer (read at correlation). Process-wide because the two workers share no other path.
static TIER_A_PADS: Mutex<Vec<TierAPad>> = Mutex::new(Vec::new());

pub(crate) fn register_tier_a(index: u8, hid_path: Option<String>, bluetooth: bool) {
    let mut pads = TIER_A_PADS.lock().unwrap();
    pads.retain(|p| p.index != index);
    pads.push(TierAPad {
        index,
        hid_path,
        bluetooth,
    });
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

/// The rumble SDL plays per wire pad: `deadline_ms << 16 | left << 8 | right`. A Bluetooth
/// media report re-selects the coil source, so it carries these levels rather than stop them.
static RUMBLE: [AtomicU64; MAX_PADS] = [const { AtomicU64::new(0) }; MAX_PADS];

/// Mirror an SDL rumble on `pad`: the levels SDL sends (`>> 8`) until its duration runs out.
pub(crate) fn note_rumble(pad: u8, low: u16, high: u16, ms: u32) {
    if let Some(slot) = RUMBLE.get(pad as usize) {
        let until = rumble_clock() + u64::from(ms);
        let levels = u64::from(low >> 8) << 8 | u64::from(high >> 8);
        slot.store(until << 16 | levels, Ordering::Relaxed);
    }
}

/// `(left, right)` while a mirrored rumble still runs.
fn rumble_now(pad: u8) -> Option<(u8, u8)> {
    let v = RUMBLE.get(pad as usize)?.load(Ordering::Relaxed);
    let (left, right) = ((v >> 8) as u8, v as u8);
    (rumble_clock() < v >> 16 && (left, right) != (0, 0)).then_some((left, right))
}

fn rumble_clock() -> u64 {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    EPOCH.get_or_init(Instant::now).elapsed().as_millis() as u64
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

/// Pad-audio renderer: 0xD1 consumer. Opens the sink on the first frame so a session without
/// a DualSense is an idle 10 ms poll. Exits on the session stop flag or the plane closing.
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
    // v1 renders the first streaming pad, so one decode stage serves it.
    let mut stage = PadDecode::new(haptics, speaker);
    let mut mixer = QuadMixer::<f32>::new(MAX_BUFFER_FRAMES);
    let mut pcm = vec![0f32; MAX_FRAME_SAMPLES * 2];
    let mut out: Option<Sink> = None;
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
        if !stage.wants(f.kind) {
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
        // Rendered haptics take the coils from wire rumble (`haptics_live`).
        if is_haptics_evidence(&f, out.is_some()) {
            HAPTICS.note(f.pad);
        }
        stage.decode_frame(&f, &mut pcm, &mut mixer);
        // Open lazily; drop + re-correlate with backoff when the sink vanishes.
        if out.as_ref().is_some_and(Sink::finished) {
            tracing::info!("pad-audio output ended (device gone?) — re-correlating");
            out = None;
            retry_at = Instant::now() + backoff;
            backoff = (backoff * 2).min(RETRY_MAX);
        }
        if out.is_none() && Instant::now() >= retry_at {
            match Sink::open(f.pad) {
                Ok(o) => {
                    tracing::info!(
                        bluetooth = matches!(o, Sink::Bluetooth(_)),
                        "pad-audio output opened on the DualSense"
                    );
                    out = Some(o);
                    backoff = RETRY_MIN;
                    open_fail_logged = false;
                }
                Err(e) => {
                    if !open_fail_logged {
                        open_fail_logged = true;
                        tracing::warn!(
                            error = %format!("{e:#}"),
                            "no DualSense audio output — pad audio parked (retrying with backoff)"
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
    usb::restore_profile();
    tracing::debug!("pad-audio pull thread exited");
}

/// Where the mixed four-channel stream plays: the pad's USB audio device or its Bluetooth link.
enum Sink {
    Usb(PadOut),
    Bluetooth(bluetooth::BtOut),
}

impl Sink {
    /// `pad` is the latched wire pad; its Bluetooth registration picks the HID link.
    fn open(pad: u8) -> anyhow::Result<Sink> {
        let bt_path = TIER_A_PADS
            .lock()
            .unwrap()
            .iter()
            .find(|p| p.index == pad && p.bluetooth)
            .and_then(|p| p.hid_path.clone());
        match bt_path {
            Some(path) => bluetooth::BtOut::open(&path, pad).map(Sink::Bluetooth),
            None => PadOut::open().map(Sink::Usb),
        }
    }

    fn take_buffer(&self) -> Vec<f32> {
        match self {
            Sink::Usb(o) => o.take_buffer(),
            Sink::Bluetooth(o) => o.take_buffer(),
        }
    }

    fn push(&self, pcm: Vec<f32>) {
        match self {
            Sink::Usb(o) => o.push(pcm),
            Sink::Bluetooth(o) => o.push(pcm),
        }
    }

    fn finished(&self) -> bool {
        match self {
            Sink::Usb(o) => o.finished(),
            Sink::Bluetooth(o) => o.finished(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speaker_mode_gates() {
        assert!(speaker_active("pad"));
        assert!(!speaker_active("off"));
        assert!(!speaker_active("mix"));
        assert!(!speaker_active(""));
        assert!(!speaker_active("Pad")); // stored names are lowercase
    }

    #[test]
    fn tier_a_is_ds5_or_edge_only() {
        assert!(is_tier_a_ds5(0x054C, 0x0CE6));
        assert!(is_tier_a_ds5(0x054C, 0x0DF2));
        assert!(!is_tier_a_ds5(0x054C, 0x05C4));
        assert!(!is_tier_a_ds5(0x045E, 0x0CE6));
        assert!(!is_tier_a_ds5(0x28DE, 0x1205));
    }

    #[test]
    fn rumble_mirror_runs_until_its_duration() {
        note_rumble(3, 0x4000, 0x8000, 60_000);
        assert_eq!(rumble_now(3), Some((0x40, 0x80)));
        note_rumble(3, 0x4000, 0x8000, 0);
        assert_eq!(rumble_now(3), None, "expired");
        note_rumble(3, 0, 0, 60_000);
        assert_eq!(rumble_now(3), None, "zero levels are no rumble");
        assert_eq!(rumble_now(200), None, "out of range");
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
