//! Pure WASAPI render-endpoint assignment for [`audio_control`](super::audio_control).
//!
//! Compiles and tests on every platform so a precedence regression fails CI on Linux too.
//!
//! Two jobs share the render set and must never land on the same endpoint: WASAPI
//! loopback recaptures whatever the virtual mic writes (echo). Tier-0 ([`MintedIds`])
//! is identity, not name — minted instances share Steam's friendly names. Below that
//! the mic is assigned first (a cable can never be a loopback), except that game audio
//! outranks the optional mic: the Steam Streaming Microphone render side is also the
//! only silent client-only sink, so the mic may take it only while the loopback still
//! gets a preferred pick ([`Wiring::mic_withheld`]). `PUNKTFUNK_MIC_DEVICE` still wins.
//!
//! Default loopback prefers a silent working sink (Streaming Microphone, not Speakers);
//! `host_audio` flips to real hardware. Speakers are a flagged last resort (silent
//! loopback). Cables / VoiceMeeter / generic "virtual" are never a last resort (echo).
//! Callers wait on [`fingerprint`], not retry. Pin via the unit tests in this file.

/// WASAPI `(friendly_name, endpoint_id)`.
pub type Endpoint = (String, String);

/// Engine mix format from `IAudioClient::GetMixFormat`.
///
/// Capture opens with `autoconvert: true` at 48 kHz f32 wire layout, so WASAPI
/// converts silently. The logged "48 kHz f32 stereo" is the REQUEST, not the
/// source — a 24 kHz mono mix is already bottlenecked before Opus sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MixFormat {
    pub rate_hz: u32,
    pub channels: u16,
    pub bits: u16,
}

impl MixFormat {
    /// Why this mix would narrow a `want`-channel desktop stream, or `None`.
    /// Bit depth is not a criterion: 16-bit is ~96 dB, below Opus's noise floor.
    pub fn narrowing(&self, want: u8) -> Option<String> {
        if self.rate_hz < 48_000 && self.channels < want as u16 {
            return Some(format!(
                "mixes at {} Hz and only {} channel(s)",
                self.rate_hz, self.channels
            ));
        }
        if self.rate_hz < 48_000 {
            return Some(format!(
                "mixes at {} Hz, so the stream is band-limited to ~{} kHz before Opus sees it",
                self.rate_hz,
                self.rate_hz / 2000
            ));
        }
        if self.channels < want as u16 {
            return Some(format!(
                "mixes {} channel(s), so a {want}-channel desktop mix is downmixed and re-expanded",
                self.channels
            ));
        }
        None
    }
}

/// Mix-format lookup. `None` = unknown, treated as intact so a probe failure
/// cannot make the plan worse than the format-blind path.
pub type FormatProbe<'a> = &'a dyn Fn(&Endpoint) -> Option<MixFormat>;

pub fn no_formats(_: &Endpoint) -> Option<MixFormat> {
    None
}

/// Host-minted Valve streaming-driver instances, keyed by WASAPI endpoint id.
///
/// Tier-0 is identity, not name: a minted instance is indistinguishable by
/// friendly name from Steam's primaries, so the provider records ids and the
/// plan matches those. Empty fields fall through to the name ladder.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct MintedIds {
    /// Reserved client-only loopback sink. Steam Remote Play never contends it.
    pub speakers_render: Option<String>,
    pub mic_render: Option<String>,
    pub mic_capture: Option<String>,
}

/// Whether desktop audio and the mic both have endpoints after a plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioReadiness {
    Full,
    AudioOnly,
    MicOnly,
    Nothing,
}

pub fn readiness(w: &Wiring) -> AudioReadiness {
    match (w.loopback_render.is_some(), w.mic_render.is_some()) {
        (true, true) => AudioReadiness::Full,
        (true, false) => AudioReadiness::AudioOnly,
        (false, true) => AudioReadiness::MicOnly,
        (false, false) => AudioReadiness::Nothing,
    }
}

/// Endpoint assignment for one wiring pass. Recomputed on every mic/capture
/// (re)open — Windows endpoints churn, so a once-per-process plan goes stale.
#[derive(Debug, Clone, PartialEq)]
pub struct Wiring {
    /// Virtual-mic write target. The loopback must never capture this device.
    pub mic_render: Option<Endpoint>,
    /// Capture side of the mic device; parked as the default recording device.
    pub mic_capture: Option<Endpoint>,
    /// Desktop-audio loopback source; parked as the default playback device.
    pub loopback_render: Option<Endpoint>,
    /// `loopback_render` is the Steam Streaming Speakers (known-silent loopback),
    /// taken only because nothing better survived. Capture treats it as a stopgap.
    pub loopback_last_resort: bool,
    /// Chosen loopback's mix format narrows the desktop mix ([`MixFormat::narrowing`]);
    /// taken because nothing better existed. Quality risk, not a failure.
    pub loopback_narrowing: Option<String>,
    /// Mic was denied the Streaming Microphone so the loopback could keep a
    /// preferred pick. `mic_render` may still hold a lesser candidate.
    pub mic_withheld: bool,
}

impl Wiring {
    /// No loopback endpoint, not even last resort. [`plan`] is pure, so this is
    /// structural: retry without a [`fingerprint`] change repeats the same verdict.
    pub fn loopback_unsatisfiable(&self) -> bool {
        self.loopback_render.is_none()
    }
}

/// Name-ladder mic write targets (lowercased), below minted tier-0. Cable first:
/// a user-installed VB-CABLE still serves and can never be a loopback.
const MIC_CANDIDATES: &[&str] = &[
    "cable input", // VB-Audio Virtual Cable
    "steam streaming microphone",
    "voicemeeter input",
    "voicemeeter aux input",
    "virtual",
];

/// Capture-side name needles for the audio written into a given mic render.
fn capture_for(mic_render_lname: &str) -> &'static [&'static str] {
    if mic_render_lname.contains("cable") {
        &["cable output"]
    } else if mic_render_lname.contains("steam streaming microphone") {
        &["steam streaming microphone"]
    } else if mic_render_lname.contains("voicemeeter") {
        &["voicemeeter out", "voicemeeter"]
    } else {
        &["virtual"]
    }
}

/// Render endpoints a loopback must not capture: cables (echo even if unused),
/// Steam Streaming Speakers (silent loopback), VoiceMeeter (one shared mixer —
/// any strip recaptures the mic), and generic "virtual". Also the capture
/// watchdog's "this new default can never work" test.
pub fn excluded_from_loopback(lname: &str) -> bool {
    lname.contains("cable")
        || lname.contains("steam streaming speakers")
        || lname.contains("voicemeeter")
        || lname.contains("virtual")
}

/// Silent on the host but loopback-capturable. Only the Streaming Microphone
/// render side qualifies (the Speakers' loopback is silent).
pub fn silent_sink(lname: &str) -> bool {
    lname.contains("steam streaming microphone")
}

/// Capture side of a virtual device. Recording-default hygiene must never
/// park the box's default on one of these.
pub fn virtual_capture(lname: &str) -> bool {
    lname.contains("cable output")
        || lname.contains("steam streaming")
        || lname.contains("voicemeeter")
        || lname.contains("virtual")
        || lname.contains("punktfunk")
}

/// First real capture endpoint (skip `avoid_id` and every [`virtual_capture`]).
/// `None` when there is no real microphone: leave the default alone.
pub fn real_capture<'a>(captures: &'a [Endpoint], avoid_id: Option<&str>) -> Option<&'a Endpoint> {
    captures
        .iter()
        .find(|(n, id)| Some(id.as_str()) != avoid_id && !virtual_capture(&n.to_lowercase()))
}

/// Known-virtual render. A name without these markers is real hardware.
fn virtualish(lname: &str) -> bool {
    lname.contains("virtual")
        || lname.contains("cable")
        || lname.contains("steam streaming")
        || lname.contains("voicemeeter")
}

/// Whether this render id is a pad-audio endpoint the provisioner minted.
///
/// Pad endpoints are stamped with the controller name so games treat them as
/// the pad speaker; no name rule can recognise one. Match the provisioner's ids.
pub fn is_pad_render(id: &str, pad_renders: &[String]) -> bool {
    pad_renders.iter().any(|p| p == id)
}

/// Assign endpoints. `mic_want` (`PUNKTFUNK_MIC_DEVICE`, lowercased) beats the
/// mic ladder. `host_audio` prefers real hardware; default prefers the silent sink.
pub fn plan(
    renders: &[Endpoint],
    captures: &[Endpoint],
    mic_want: Option<&str>,
    host_audio: bool,
    pad_renders: &[String],
    minted: &MintedIds,
) -> Wiring {
    plan_with_formats(
        renders,
        captures,
        mic_want,
        host_audio,
        &no_formats,
        2,
        pad_renders,
        minted,
    )
}

/// [`plan`] plus each render's engine mix format and the session channel count.
///
/// A silent sink must earn preference: if [`MixFormat::narrowing`] fires it
/// drops below real hardware. Still taken when nothing better exists (narrow
/// beats none) and flagged in [`Wiring::loopback_narrowing`]. Unknown format
/// counts as intact so a probe failure cannot make the plan worse.
#[allow(clippy::too_many_arguments)] // mirrors the enumeration inputs; a param struct would only rename the problem
pub fn plan_with_formats(
    renders: &[Endpoint],
    captures: &[Endpoint],
    mic_want: Option<&str>,
    host_audio: bool,
    format_of: FormatProbe,
    want_channels: u8,
    pad_renders: &[String],
    minted: &MintedIds,
) -> Wiring {
    // Pad endpoints are invisible: not a mic (voice would play from the pad),
    // not a loopback (controller cues would stream as desktop), not last resort.
    // Names look like real hardware ("DualSense Wireless Controller" on purpose).
    let renders: Vec<Endpoint> = renders
        .iter()
        .filter(|(_, id)| !is_pad_render(id, pad_renders))
        .cloned()
        .collect();
    let renders = renders.as_slice();
    let find_render = |needle: &str| {
        renders
            .iter()
            .find(|(n, _)| n.to_lowercase().contains(needle))
            .cloned()
    };

    // Resolve minted ids against THIS enumeration; a stale id must not phantom.
    let find_by_id = |id: &Option<String>| -> Option<Endpoint> {
        id.as_deref()
            .and_then(|id| renders.iter().find(|(_, rid)| rid == id).cloned())
    };
    let minted_mic = find_by_id(&minted.mic_render);
    let minted_sink = find_by_id(&minted.speakers_render);

    // Mic first (narrower: must be a virtual cable). Minted mic outranks every
    // name candidate and never costs the loopback (minted sink is its pair).
    // Operator override still beats it.
    let mic_render = match mic_want {
        Some(w) => find_render(w),
        None => minted_mic
            .clone()
            .or_else(|| MIC_CANDIDATES.iter().find_map(|c| find_render(c))),
    };
    // Game audio outranks the optional mic: Streaming Microphone render is also
    // the only silent client-only sink. Keep it only while loopback still gets a
    // preferred pick without it. Override is exempt (`PUNKTFUNK_MIC_DEVICE`).
    let mut mic_withheld = false;
    let mic_render = match mic_render {
        Some((name, id)) if mic_want.is_none() && silent_sink(&name.to_lowercase()) => {
            let loopback_survives = renders.iter().any(|(n, rid)| {
                let ln = n.to_lowercase();
                *rid != id
                    && (silent_sink(&ln) || (!excluded_from_loopback(&ln) && !virtualish(&ln)))
            });
            if loopback_survives {
                Some((name, id))
            } else {
                mic_withheld = true;
                // Fall to a lesser candidate; the silent sink goes to loopback.
                MIC_CANDIDATES
                    .iter()
                    .filter(|c| !silent_sink(c))
                    .find_map(|c| find_render(c))
            }
        }
        other => other,
    };

    // Capture side. A minted mic pairs by recorded CAPTURE id — names cannot
    // tell it from Steam's primary, and pairing the primary would record silence.
    let mic_capture = mic_render.as_ref().and_then(|(name, id)| {
        if Some(id) == minted.mic_render.as_ref() {
            return minted
                .mic_capture
                .as_deref()
                .and_then(|cid| captures.iter().find(|(_, c)| c == cid).cloned());
        }
        capture_for(&name.to_lowercase()).iter().find_map(|c| {
            captures
                .iter()
                .find(|(n, _)| n.to_lowercase().contains(c))
                .cloned()
        })
    });

    // Loopback from remaining renders. Default: silent sink > real hardware.
    // `host_audio`: hardware first. Both can fall through to last resort.
    let not_mic = |id: &str| mic_render.as_ref().is_none_or(|(_, mid)| mid != id);
    let real_hw = || {
        renders.iter().find(|(n, id)| {
            let ln = n.to_lowercase();
            not_mic(id) && !excluded_from_loopback(&ln) && !virtualish(&ln)
        })
    };
    // Intact silent sink keeps preference; a narrowing one falls below hardware.
    let narrowing_of = |ep: &Endpoint| format_of(ep).and_then(|f| f.narrowing(want_channels));
    let silent_intact = || {
        renders.iter().find(|ep| {
            not_mic(&ep.1) && silent_sink(&ep.0.to_lowercase()) && narrowing_of(ep).is_none()
        })
    };
    let silent_narrow = || {
        renders.iter().find(|ep| {
            not_mic(&ep.1) && silent_sink(&ep.0.to_lowercase()) && narrowing_of(ep).is_some()
        })
    };
    // Last resort: Steam Streaming Speakers only. Silent loopback is a quality
    // risk; a parked endpoint beats none. Never cables/VoiceMeeter/"virtual"
    // (echo). Leave `excluded_from_loopback` alone — the watchdog uses it for
    // a NEW operator default, and admitting Speakers would change snap-back.
    let last_resort = || {
        renders
            .iter()
            .find(|(n, id)| not_mic(id) && n.to_lowercase().contains("steam streaming speakers"))
    };
    // Minted Speakers: same quality discipline as any silent sink — narrowing
    // demotes below real hardware rather than silently costing quality.
    let minted_intact = || {
        minted_sink
            .as_ref()
            .filter(|(_, id)| not_mic(id))
            .filter(|ep| narrowing_of(ep).is_none())
    };
    let minted_narrow = || {
        minted_sink
            .as_ref()
            .filter(|(_, id)| not_mic(id))
            .filter(|ep| narrowing_of(ep).is_some())
    };
    // Narrowing silence sits below hardware in BOTH modes. The minted sink
    // still heads its tier — that is the endpoint whose purpose is this role.
    let preferred = if host_audio {
        real_hw()
            .or_else(minted_intact)
            .or_else(silent_intact)
            .or_else(minted_narrow)
            .or_else(silent_narrow)
    } else {
        minted_intact()
            .or_else(silent_intact)
            .or_else(real_hw)
            .or_else(minted_narrow)
            .or_else(silent_narrow)
    };
    let (loopback_render, loopback_last_resort) = match preferred {
        Some(ep) => (Some(ep.clone()), false),
        None => match last_resort() {
            Some(ep) => (Some(ep.clone()), true),
            None => (None, false),
        },
    };
    // Flag whatever we chose, including real hardware (headset hands-free is 16 kHz mono).
    let loopback_narrowing = loopback_render.as_ref().and_then(narrowing_of);

    Wiring {
        mic_render,
        mic_capture,
        loopback_render,
        loopback_last_resort,
        loopback_narrowing,
        mic_withheld,
    }
}

/// Order-independent fingerprint of an enumerated set. [`plan`] is a pure
/// function of these inputs, so an unchanged fingerprint proves an unchanged
/// verdict. Capture polls this instead of re-planning (IPolicyConfig writes
/// are the side effect of a needless re-plan).
pub fn fingerprint(renders: &[Endpoint], captures: &[Endpoint]) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    // Hash a sorted Vec per direction so order churn is ignored and renders
    // cannot alias captures (`Vec`'s hash is length-prefixed).
    for eps in [renders, captures] {
        let mut sorted: Vec<&Endpoint> = eps.iter().collect();
        sorted.sort();
        sorted.hash(&mut h);
    }
    h.finish()
}

/// Why every enumerated render was rejected, then only remedies not already
/// taken. Static "install Steam" advice is wrong when the pair is installed
/// but reserved. Pure: same enumeration [`plan`] consumed.
pub fn describe_no_loopback(renders: &[Endpoint], wiring: &Wiring) -> String {
    debug_assert!(wiring.loopback_unsatisfiable());
    let mic_id = wiring.mic_render.as_ref().map(|(_, id)| id.as_str());
    let rejected: Vec<String> = renders
        .iter()
        .map(|(name, id)| {
            let ln = name.to_lowercase();
            let why = if Some(id.as_str()) == mic_id {
                "reserved for the virtual mic (its loopback would echo the client's voice back)"
            } else if ln.contains("cable") {
                "virtual cable (its loopback re-captures what is written into it)"
            } else if ln.contains("voicemeeter") {
                "VoiceMeeter strip (shares the mixer the mic writes into — a feedback loop)"
            } else if ln.contains("steam streaming speakers") {
                // Only when Speakers ARE the mic target (override) — last resort takes them otherwise.
                "known-silent loopback (validated live)"
            } else if ln.contains("virtual") {
                "unrecognized virtual endpoint (assumed feedback/silence risk)"
            } else {
                // `plan` accepts any non-virtual render; this arm means a tier drifted.
                "rejected by the wiring plan"
            };
            format!("{name:?}: {why}")
        })
        .collect();
    let inventory = if rejected.is_empty() {
        "no render endpoints exist at all".to_string()
    } else {
        rejected.join("; ")
    };
    let has = |needle: &str| {
        renders
            .iter()
            .any(|(n, _)| n.to_lowercase().contains(needle))
    };
    let mut remedies = vec!["attach any output device (headphones, or a monitor/TV with audio)"];
    // Only when a cable would free a loopback-capable endpoint (mic on the
    // Streaming Microphone). A VoiceMeeter mic frees nothing capturable.
    if !has("cable")
        && wiring
            .mic_render
            .as_ref()
            .is_some_and(|(n, _)| silent_sink(&n.to_lowercase()))
    {
        remedies.push(
            "install VB-Audio Virtual Cable — the mic then takes the cable and frees the Steam \
             Streaming Microphone's render side for the loopback",
        );
    }
    if !has("steam streaming microphone") {
        remedies.push(
            "install Steam — its Remote Play streaming drivers add a loopback-capable virtual \
             sink",
        );
    }
    format!(
        "no loopback-capturable render endpoint: {inventory}. Remedies: {}",
        remedies.join("; or ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ep(name: &str) -> Endpoint {
        (name.to_string(), format!("id-{}", name.to_lowercase()))
    }

    #[test]
    fn gaming_pc_with_cable() {
        let renders = [
            ep("Speakers (Realtek HD Audio)"),
            ep("CABLE Input (VB-Audio Virtual Cable)"),
        ];
        let captures = [
            ep("Microphone (Webcam)"),
            ep("CABLE Output (VB-Audio Virtual Cable)"),
        ];
        let w = plan(&renders, &captures, None, false, &[], &MintedIds::default());
        assert_eq!(
            w.mic_render.unwrap().0,
            "CABLE Input (VB-Audio Virtual Cable)"
        );
        assert_eq!(
            w.mic_capture.unwrap().0,
            "CABLE Output (VB-Audio Virtual Cable)"
        );
        assert_eq!(w.loopback_render.unwrap().0, "Speakers (Realtek HD Audio)");
    }

    #[test]
    fn client_only_prefers_silent_sink_over_hardware() {
        let renders = [
            ep("Speakers (Apple Audio Device)"),
            ep("CABLE Input (VB-Audio Virtual Cable)"),
            ep("Speakers (Steam Streaming Speakers)"),
            ep("CABLE In 16ch (VB-Audio Virtual Cable)"),
            ep("Speakers (Steam Streaming Microphone)"),
        ];
        let captures = [
            ep("CABLE Output (VB-Audio Virtual Cable)"),
            ep("Microphone (Steam Streaming Microphone)"),
        ];
        let w = plan(&renders, &captures, None, false, &[], &MintedIds::default());
        assert_eq!(
            w.mic_render.unwrap().0,
            "CABLE Input (VB-Audio Virtual Cable)"
        );
        assert_eq!(
            w.loopback_render.unwrap().0,
            "Speakers (Steam Streaming Microphone)"
        );
    }

    #[test]
    fn host_audio_prefers_real_hardware() {
        let renders = [
            ep("Speakers (Apple Audio Device)"),
            ep("CABLE Input (VB-Audio Virtual Cable)"),
            ep("Speakers (Steam Streaming Microphone)"),
        ];
        let w = plan(&renders, &[], None, true, &[], &MintedIds::default());
        assert_eq!(
            w.loopback_render.unwrap().0,
            "Speakers (Apple Audio Device)"
        );
    }

    /// "CABLE In 16ch" feeds the same CABLE Output as the mic. Capturing it is
    /// silence (nothing renders there), never a loopback in either mode.
    #[test]
    fn cable_16ch_never_loopback() {
        let renders = [
            ep("CABLE Input (VB-Audio Virtual Cable)"),
            ep("CABLE In 16ch (VB-Audio Virtual Cable)"),
        ];
        for host_audio in [false, true] {
            let w = plan(&renders, &[], None, host_audio, &[], &MintedIds::default());
            assert!(w.loopback_render.is_none(), "host_audio={host_audio}");
        }
    }

    /// Cable is the only render (hence the default). Mic wins; loopback absent.
    /// Rejecting the cable here permanently kills mic passthrough.
    #[test]
    fn headless_cable_only_mic_wins() {
        let renders = [ep("CABLE Input (VB-Audio Virtual Cable)")];
        let captures = [ep("CABLE Output (VB-Audio Virtual Cable)")];
        let w = plan(&renders, &captures, None, false, &[], &MintedIds::default());
        assert!(w.mic_render.is_some(), "mic must claim the only cable");
        assert!(w.loopback_render.is_none(), "no echo-safe loopback exists");
    }

    #[test]
    fn headless_with_steam_pair() {
        let renders = [
            ep("CABLE Input (VB-Audio Virtual Cable)"),
            ep("Speakers (Steam Streaming Speakers)"),
            ep("Speakers (Steam Streaming Microphone)"),
        ];
        let captures = [
            ep("CABLE Output (VB-Audio Virtual Cable)"),
            ep("Microphone (Steam Streaming Microphone)"),
        ];
        let w = plan(&renders, &captures, None, false, &[], &MintedIds::default());
        assert_eq!(
            w.mic_render.unwrap().0,
            "CABLE Input (VB-Audio Virtual Cable)"
        );
        assert_eq!(
            w.loopback_render.unwrap().0,
            "Speakers (Steam Streaming Microphone)"
        );
        assert!(
            !w.loopback_last_resort,
            "the silent sink is a PREFERRED pick"
        );
        assert_eq!(
            w.mic_capture.unwrap().0,
            "CABLE Output (VB-Audio Virtual Cable)"
        );
    }

    #[test]
    fn steam_mic_as_target_never_doubles_as_loopback() {
        let renders = [
            ep("Speakers (Steam Streaming Microphone)"),
            ep("Speakers (Realtek HD Audio)"),
        ];
        let captures = [ep("Microphone (Steam Streaming Microphone)")];
        let w = plan(&renders, &captures, None, false, &[], &MintedIds::default());
        assert_eq!(
            w.mic_render.unwrap().0,
            "Speakers (Steam Streaming Microphone)"
        );
        assert!(!w.mic_withheld);
        assert_eq!(w.loopback_render.unwrap().0, "Speakers (Realtek HD Audio)");
    }

    #[test]
    fn steam_mic_only_audio_wins() {
        let renders = [ep("Speakers (Steam Streaming Microphone)")];
        let captures = [ep("Microphone (Steam Streaming Microphone)")];
        let w = plan(&renders, &captures, None, false, &[], &MintedIds::default());
        assert!(w.mic_render.is_none());
        assert!(w.mic_withheld);
        assert_eq!(
            w.loopback_render.unwrap().0,
            "Speakers (Steam Streaming Microphone)"
        );
        assert!(!w.loopback_last_resort);
    }

    #[test]
    fn withheld_mic_falls_to_voicemeeter() {
        let renders = [
            ep("Speakers (Steam Streaming Speakers)"),
            ep("Speakers (Steam Streaming Microphone)"),
            ep("Voicemeeter Input (VB-Audio Voicemeeter VAIO)"),
        ];
        let captures = [
            ep("Microphone (Steam Streaming Microphone)"),
            ep("Voicemeeter Out B1 (VB-Audio Voicemeeter VAIO)"),
        ];
        let w = plan(&renders, &captures, None, false, &[], &MintedIds::default());
        assert_eq!(
            w.mic_render.as_ref().unwrap().0,
            "Voicemeeter Input (VB-Audio Voicemeeter VAIO)"
        );
        assert!(w.mic_withheld);
        assert_eq!(
            w.mic_capture.unwrap().0,
            "Voicemeeter Out B1 (VB-Audio Voicemeeter VAIO)"
        );
        assert_eq!(
            w.loopback_render.unwrap().0,
            "Speakers (Steam Streaming Microphone)"
        );
        assert!(!w.loopback_last_resort);
    }

    #[test]
    fn steam_speakers_only_as_last_resort() {
        let renders = [
            ep("CABLE Input (VB-Audio Virtual Cable)"),
            ep("Speakers (Steam Streaming Speakers)"),
        ];
        for host_audio in [false, true] {
            let w = plan(&renders, &[], None, host_audio, &[], &MintedIds::default());
            assert_eq!(
                w.loopback_render.as_ref().unwrap().0,
                "Speakers (Steam Streaming Speakers)",
                "host_audio={host_audio}"
            );
            assert!(w.loopback_last_resort, "host_audio={host_audio}");
        }
    }

    #[test]
    fn field_case_steam_pair_only_audio_outranks_mic() {
        let renders = [
            ep("Altavoces (Steam Streaming Speakers)"),
            ep("Altavoces (Steam Streaming Microphone)"),
        ];
        let captures = [ep("Microphone (Steam Streaming Microphone)")];
        for host_audio in [false, true] {
            let w = plan(
                &renders,
                &captures,
                None,
                host_audio,
                &[],
                &MintedIds::default(),
            );
            assert!(w.mic_render.is_none(), "host_audio={host_audio}");
            assert!(w.mic_withheld, "host_audio={host_audio}");
            assert_eq!(
                w.loopback_render.as_ref().unwrap().0,
                "Altavoces (Steam Streaming Microphone)",
                "host_audio={host_audio}"
            );
            assert!(!w.loopback_last_resort, "host_audio={host_audio}");
        }
    }

    #[test]
    fn env_override_may_strand_the_loopback() {
        let renders = [
            ep("Altavoces (Steam Streaming Speakers)"),
            ep("Altavoces (Steam Streaming Microphone)"),
        ];
        let captures = [ep("Microphone (Steam Streaming Microphone)")];
        let w = plan(
            &renders,
            &captures,
            Some("steam streaming microphone"),
            false,
            &[],
            &MintedIds::default(),
        );
        assert_eq!(
            w.mic_render.unwrap().0,
            "Altavoces (Steam Streaming Microphone)"
        );
        assert!(!w.mic_withheld);
        assert_eq!(
            w.loopback_render.unwrap().0,
            "Altavoces (Steam Streaming Speakers)"
        );
        assert!(w.loopback_last_resort);
    }

    #[test]
    fn last_resort_never_beats_real_hardware() {
        let renders = [
            ep("Speakers (Steam Streaming Microphone)"),
            ep("Speakers (Steam Streaming Speakers)"),
            ep("Speakers (Realtek HD Audio)"),
        ];
        let captures = [ep("Microphone (Steam Streaming Microphone)")];
        for host_audio in [false, true] {
            let w = plan(
                &renders,
                &captures,
                None,
                host_audio,
                &[],
                &MintedIds::default(),
            );
            assert_eq!(
                w.loopback_render.as_ref().unwrap().0,
                "Speakers (Realtek HD Audio)",
                "host_audio={host_audio}"
            );
            assert!(!w.loopback_last_resort, "host_audio={host_audio}");
        }
    }

    #[test]
    fn cable_and_voicemeeter_never_last_resort() {
        let renders = [
            ep("CABLE Input (VB-Audio Virtual Cable)"),
            ep("CABLE In 16ch (VB-Audio Virtual Cable)"),
            ep("Voicemeeter Aux Input (VB-Audio Voicemeeter AUX VAIO)"),
        ];
        let captures = [ep("CABLE Output (VB-Audio Virtual Cable)")];
        for host_audio in [false, true] {
            let w = plan(
                &renders,
                &captures,
                None,
                host_audio,
                &[],
                &MintedIds::default(),
            );
            assert!(w.loopback_render.is_none(), "host_audio={host_audio}");
            assert!(!w.loopback_last_resort, "host_audio={host_audio}");
            assert!(w.loopback_unsatisfiable(), "host_audio={host_audio}");
        }
    }

    // ---- format-aware loopback selection ------------------------------------------------

    fn fmt(rate_hz: u32, channels: u16) -> MixFormat {
        MixFormat {
            rate_hz,
            channels,
            bits: 32,
        }
    }

    /// Name-substring → format table, owned so call sites can pass a literal.
    fn probe(table: Vec<(&'static str, MixFormat)>) -> impl Fn(&Endpoint) -> Option<MixFormat> {
        move |ep: &Endpoint| {
            let name = ep.0.to_lowercase();
            table
                .iter()
                .find_map(|(needle, f)| name.contains(needle).then_some(*f))
        }
    }

    #[test]
    fn narrowing_silent_sink_loses_to_real_hardware() {
        let renders = [
            ep("CABLE In 16ch (VB-Audio Virtual Cable)"),
            ep("Altavoces (Steam Streaming Speakers)"),
            ep("Altavoces (Steam Streaming Microphone)"),
            ep("CABLE Input (VB-Audio Virtual Cable)"),
            ep("1 - Odyssey G60SD (AMD High Definition Audio Device)"),
        ];
        let captures = [
            ep("CABLE Output (VB-Audio Virtual Cable)"),
            ep("Microphone (Steam Streaming Microphone)"),
        ];
        // Voice-carrier mix: 24 kHz mono.
        let p = probe(vec![
            ("steam streaming microphone", fmt(24_000, 1)),
            ("odyssey", fmt(48_000, 2)),
        ]);
        let w = plan_with_formats(
            &renders,
            &captures,
            None,
            false,
            &p,
            2,
            &[],
            &MintedIds::default(),
        );
        assert_eq!(
            w.loopback_render.as_ref().unwrap().0,
            "1 - Odyssey G60SD (AMD High Definition Audio Device)",
            "a narrowing silent sink must not beat clean real hardware"
        );
        assert!(
            w.loopback_narrowing.is_none(),
            "the chosen endpoint is intact"
        );
        assert_eq!(
            w.mic_render.unwrap().0,
            "CABLE Input (VB-Audio Virtual Cable)"
        );
    }

    #[test]
    fn intact_silent_sink_still_wins() {
        let renders = [
            ep("Speakers (Realtek HD Audio)"),
            ep("CABLE Input (VB-Audio Virtual Cable)"),
            ep("Speakers (Steam Streaming Microphone)"),
        ];
        let p = probe(vec![
            ("steam streaming microphone", fmt(48_000, 2)),
            ("realtek", fmt(48_000, 2)),
        ]);
        let w = plan_with_formats(
            &renders,
            &[],
            None,
            false,
            &p,
            2,
            &[],
            &MintedIds::default(),
        );
        assert_eq!(
            w.loopback_render.unwrap().0,
            "Speakers (Steam Streaming Microphone)"
        );
    }

    #[test]
    fn narrowing_sink_is_taken_when_it_is_all_there_is() {
        let renders = [
            ep("CABLE Input (VB-Audio Virtual Cable)"),
            ep("Speakers (Steam Streaming Microphone)"),
        ];
        let p = probe(vec![("steam streaming microphone", fmt(16_000, 1))]);
        let w = plan_with_formats(
            &renders,
            &[],
            None,
            false,
            &p,
            2,
            &[],
            &MintedIds::default(),
        );
        assert_eq!(
            w.loopback_render.as_ref().unwrap().0,
            "Speakers (Steam Streaming Microphone)"
        );
        let why = w.loopback_narrowing.expect("must be flagged");
        assert!(why.contains("16000"), "{why}");
    }

    /// Headset hands-free is 16 kHz mono. The flag is about the chosen endpoint.
    #[test]
    fn narrowing_is_reported_for_real_hardware_too() {
        let renders = [ep("Headset (Hands-Free AG Audio)")];
        let p = probe(vec![("headset", fmt(16_000, 1))]);
        let w = plan_with_formats(
            &renders,
            &[],
            None,
            false,
            &p,
            2,
            &[],
            &MintedIds::default(),
        );
        assert_eq!(
            w.loopback_render.as_ref().unwrap().0,
            "Headset (Hands-Free AG Audio)"
        );
        assert!(w.loopback_narrowing.is_some());
    }

    #[test]
    fn unknown_formats_reproduce_the_formatless_plan() {
        let renders = [
            ep("Speakers (Apple Audio Device)"),
            ep("CABLE Input (VB-Audio Virtual Cable)"),
            ep("Speakers (Steam Streaming Speakers)"),
            ep("Speakers (Steam Streaming Microphone)"),
        ];
        let captures = [ep("CABLE Output (VB-Audio Virtual Cable)")];
        for host_audio in [false, true] {
            let a = plan(
                &renders,
                &captures,
                None,
                host_audio,
                &[],
                &MintedIds::default(),
            );
            let b = plan_with_formats(
                &renders,
                &captures,
                None,
                host_audio,
                &no_formats,
                2,
                &[],
                &MintedIds::default(),
            );
            assert_eq!(a, b, "host_audio={host_audio}");
            assert!(a.loopback_narrowing.is_none());
        }
    }

    #[test]
    fn host_audio_ordering_survives_formats() {
        let renders = [
            ep("Speakers (Realtek HD Audio)"),
            ep("Speakers (Steam Streaming Microphone)"),
        ];
        let p = probe(vec![
            ("steam streaming microphone", fmt(24_000, 1)),
            ("realtek", fmt(48_000, 2)),
        ]);
        let w = plan_with_formats(&renders, &[], None, true, &p, 2, &[], &MintedIds::default());
        assert_eq!(w.loopback_render.unwrap().0, "Speakers (Realtek HD Audio)");
    }

    #[test]
    fn narrowing_depends_on_the_session_channel_count() {
        let stereo_only = fmt(48_000, 2);
        assert_eq!(stereo_only.narrowing(2), None);
        assert!(stereo_only.narrowing(6).is_some());
        assert!(fmt(44_100, 8).narrowing(2).is_some());
        assert_eq!(fmt(48_000, 8).narrowing(2), None);
        let both = fmt(16_000, 1).narrowing(6).unwrap();
        assert!(both.contains("16000") && both.contains("channel"), "{both}");
    }

    #[test]
    fn recording_hygiene_picks_the_real_microphone() {
        let captures = [
            ep("Microphone (2- Punktfunk)"),
            ep("CABLE Output (VB-Audio Virtual Cable)"),
            ep("Microphone (Steam Streaming Microphone)"),
            ep("VoiceMeeter Output (VB-Audio VoiceMeeter VAIO)"),
            ep("Desktop Microphone (2- Microsoft LifeCam HD-3000)"),
        ];
        assert_eq!(
            real_capture(&captures, None).unwrap().0,
            "Desktop Microphone (2- Microsoft LifeCam HD-3000)"
        );
        // `avoid_id` still skips the plan's own mic capture if the name would pass.
        let only = [ep("Desk Mic (USB)")];
        assert!(real_capture(&only, Some("id-desk mic (usb)")).is_none());
        assert!(real_capture(&[], None).is_none());
    }

    #[test]
    fn env_override_wins() {
        let renders = [
            ep("CABLE Input (VB-Audio Virtual Cable)"),
            ep("Voicemeeter Input (VB-Audio Voicemeeter VAIO)"),
        ];
        let captures = [ep("Voicemeeter Out B1 (VB-Audio Voicemeeter VAIO)")];
        let w = plan(
            &renders,
            &captures,
            Some("voicemeeter input"),
            false,
            &[],
            &MintedIds::default(),
        );
        assert_eq!(
            w.mic_render.unwrap().0,
            "Voicemeeter Input (VB-Audio Voicemeeter VAIO)"
        );
        assert_eq!(
            w.mic_capture.unwrap().0,
            "Voicemeeter Out B1 (VB-Audio Voicemeeter VAIO)"
        );
    }

    #[test]
    fn no_virtual_device() {
        let renders = [ep("Speakers (Realtek HD Audio)")];
        let w = plan(&renders, &[], None, false, &[], &MintedIds::default());
        assert!(w.mic_render.is_none());
        assert_eq!(w.loopback_render.unwrap().0, "Speakers (Realtek HD Audio)");
    }

    /// VoiceMeeter strips share one mixer: capturing any strip recaptures the mic.
    #[test]
    fn voicemeeter_aux_never_pairs_with_voicemeeter_mic() {
        let renders = [
            ep("Voicemeeter Input (VB-Audio Voicemeeter VAIO)"),
            ep("Voicemeeter Aux Input (VB-Audio Voicemeeter AUX VAIO)"),
            ep("Speakers (Realtek HD Audio)"),
        ];
        let captures = [ep("Voicemeeter Out B1 (VB-Audio Voicemeeter VAIO)")];
        for host_audio in [false, true] {
            let w = plan(
                &renders,
                &captures,
                None,
                host_audio,
                &[],
                &MintedIds::default(),
            );
            assert_eq!(
                w.mic_render.as_ref().unwrap().0,
                "Voicemeeter Input (VB-Audio Voicemeeter VAIO)",
                "host_audio={host_audio}"
            );
            assert_eq!(
                w.loopback_render.as_ref().unwrap().0,
                "Speakers (Realtek HD Audio)",
                "host_audio={host_audio}"
            );
        }
    }

    #[test]
    fn voicemeeter_only_no_loopback() {
        let renders = [
            ep("Voicemeeter Input (VB-Audio Voicemeeter VAIO)"),
            ep("Voicemeeter Aux Input (VB-Audio Voicemeeter AUX VAIO)"),
        ];
        for host_audio in [false, true] {
            let w = plan(&renders, &[], None, host_audio, &[], &MintedIds::default());
            assert!(w.mic_render.is_some(), "host_audio={host_audio}");
            assert!(w.loopback_render.is_none(), "host_audio={host_audio}");
        }
    }

    #[test]
    fn unknown_virtual_never_loopback() {
        let renders = [
            ep("CABLE Input (VB-Audio Virtual Cable)"),
            ep("Speakers (Some Virtual Audio Device)"),
        ];
        let w = plan(&renders, &[], None, false, &[], &MintedIds::default());
        assert!(w.loopback_render.is_none());
    }

    #[test]
    fn fingerprint_order_independent_topology_sensitive() {
        let a = [ep("Speakers (Realtek HD Audio)"), ep("CABLE Input")];
        let a_rev = [ep("CABLE Input"), ep("Speakers (Realtek HD Audio)")];
        let caps = [ep("CABLE Output")];
        assert_eq!(fingerprint(&a, &caps), fingerprint(&a_rev, &caps));
        assert_ne!(fingerprint(&a, &caps), fingerprint(&a[..1], &caps));
        assert_ne!(fingerprint(&a, &caps), fingerprint(&caps, &a));
    }

    #[test]
    fn describe_no_loopback_skips_satisfied_remedies() {
        // Override pins the mic to Streaming Microphone — the only way it may
        // strand the loopback now that game audio outranks the candidate order.
        let renders = [ep("Altavoces (Steam Streaming Microphone)")];
        let captures = [ep("Microphone (Steam Streaming Microphone)")];
        let w = plan(
            &renders,
            &captures,
            Some("steam streaming microphone"),
            false,
            &[],
            &MintedIds::default(),
        );
        assert!(w.loopback_unsatisfiable());
        let msg = describe_no_loopback(&renders, &w);
        assert!(msg.contains("reserved for the virtual mic"), "{msg}");
        assert!(msg.contains("VB-Audio Virtual Cable"), "{msg}");
        assert!(!msg.contains("install Steam"), "{msg}");

        // Cable already present; Steam is the remedy that adds a capturable sink.
        let renders = [ep("CABLE Input (VB-Audio Virtual Cable)")];
        let captures = [ep("CABLE Output (VB-Audio Virtual Cable)")];
        let w = plan(&renders, &captures, None, false, &[], &MintedIds::default());
        assert!(w.loopback_unsatisfiable());
        let msg = describe_no_loopback(&renders, &w);
        assert!(msg.contains("install Steam"), "{msg}");
        assert!(!msg.contains("install VB-Audio Virtual Cable"), "{msg}");
    }

    /// Pad endpoints carry no virtual marker (games must read them as the pad
    /// speaker), so only id exclusion stops the name rules treating them as hardware.
    #[test]
    fn pad_endpoints_invisible() {
        let renders = [
            ep("DualSense Wireless Controller"),
            ep("Speakers (Realtek HD Audio)"),
        ];
        let pads = [renders[0].1.clone()];
        let w = plan(&renders, &[], None, false, &pads, &MintedIds::default());
        assert_eq!(w.loopback_render.unwrap().0, "Speakers (Realtek HD Audio)");
        // Override matching the pad name must not claim it either.
        let w = plan(
            &renders[..1],
            &[],
            Some("wireless controller"),
            false,
            &pads,
            &MintedIds::default(),
        );
        assert!(w.mic_render.is_none());
        assert!(w.loopback_render.is_none());
    }

    /// Last resort matches Steam Speakers by name on the same shadowed `renders`,
    /// so a pad must not be reachable there either (desktop mix into the coils).
    #[test]
    fn a_pad_is_never_the_last_resort() {
        // Pad + Steam pair, mic pinned by override so the plan falls to last resort.
        let renders = [
            ep("DualSense Wireless Controller"),
            ep("Speakers (Steam Streaming Microphone)"),
            ep("Speakers (Steam Streaming Speakers)"),
        ];
        let captures = [ep("Microphone (Steam Streaming Microphone)")];
        let pads = [renders[0].1.clone()];
        let w = plan(
            &renders,
            &captures,
            Some("steam streaming microphone"),
            false,
            &pads,
            &MintedIds::default(),
        );
        assert_eq!(
            w.loopback_render.as_ref().unwrap().0,
            "Speakers (Steam Streaming Speakers)",
            "the last resort must skip the pad"
        );
        assert!(w.loopback_last_resort);

        // Pad alone: honestly unsatisfiable, never the coils.
        let w = plan(
            &renders[..1],
            &captures,
            None,
            false,
            &pads,
            &MintedIds::default(),
        );
        assert!(
            w.loopback_render.is_none(),
            "a pad was taken as the last resort"
        );
        assert!(!w.loopback_last_resort);
        assert!(w.loopback_unsatisfiable());
    }

    // ---- minted tier-0 ------------------------------------------------------------------

    /// Primaries plus name-identical minted instances (the driver produces that).
    fn minted_zoo() -> ([Endpoint; 6], [Endpoint; 3], MintedIds) {
        let renders = [
            ep("Speakers (Realtek HD Audio)"),
            ep("CABLE Input (VB-Audio Virtual Cable)"),
            ep("Lautsprecher (Steam Streaming Speakers)"),
            ep("Lautsprecher (Steam Streaming Microphone)"),
            (
                "Lautsprecher (Steam Streaming Speakers)".into(),
                "id-minted-spk".into(),
            ),
            (
                "Lautsprecher (Steam Streaming Microphone)".into(),
                "id-minted-mic-r".into(),
            ),
        ];
        let captures = [
            ep("CABLE Output (VB-Audio Virtual Cable)"),
            ep("Mikrofon (Steam Streaming Microphone)"),
            (
                "Mikrofon (Steam Streaming Microphone)".into(),
                "id-minted-mic-c".into(),
            ),
        ];
        let minted = MintedIds {
            speakers_render: Some("id-minted-spk".into()),
            mic_render: Some("id-minted-mic-r".into()),
            mic_capture: Some("id-minted-mic-c".into()),
        };
        (renders, captures, minted)
    }

    #[test]
    fn minted_pair_is_tier_zero() {
        let (renders, captures, minted) = minted_zoo();
        let w = plan(&renders, &captures, None, false, &[], &minted);
        assert_eq!(w.mic_render.as_ref().unwrap().1, "id-minted-mic-r");
        assert_eq!(
            w.mic_capture.as_ref().unwrap().1,
            "id-minted-mic-c",
            "the capture side must pair by the provider's id, never by name"
        );
        assert_eq!(w.loopback_render.as_ref().unwrap().1, "id-minted-spk");
        assert!(!w.loopback_last_resort);
        assert!(!w.mic_withheld);
        assert_eq!(readiness(&w), AudioReadiness::Full);
    }

    #[test]
    fn minted_host_audio_prefers_hardware() {
        let (renders, captures, minted) = minted_zoo();
        let w = plan(&renders, &captures, None, true, &[], &minted);
        assert_eq!(w.mic_render.as_ref().unwrap().1, "id-minted-mic-r");
        assert_eq!(w.loopback_render.unwrap().0, "Speakers (Realtek HD Audio)");
    }

    #[test]
    fn env_override_beats_minted() {
        let (renders, captures, minted) = minted_zoo();
        let w = plan(
            &renders,
            &captures,
            Some("cable input"),
            false,
            &[],
            &minted,
        );
        assert_eq!(
            w.mic_render.unwrap().0,
            "CABLE Input (VB-Audio Virtual Cable)"
        );
        assert_eq!(w.loopback_render.unwrap().1, "id-minted-spk");
    }

    #[test]
    fn minted_speakers_only_mic_uses_ladder() {
        let (renders, captures, mut minted) = minted_zoo();
        minted.mic_render = None;
        minted.mic_capture = None;
        let w = plan(&renders, &captures, None, false, &[], &minted);
        assert_eq!(
            w.mic_render.unwrap().0,
            "CABLE Input (VB-Audio Virtual Cable)"
        );
        assert_eq!(w.loopback_render.unwrap().1, "id-minted-spk");
    }

    #[test]
    fn stale_minted_ids_fall_back() {
        let renders = [
            ep("Speakers (Realtek HD Audio)"),
            ep("CABLE Input (VB-Audio Virtual Cable)"),
        ];
        let captures = [ep("CABLE Output (VB-Audio Virtual Cable)")];
        let minted = MintedIds {
            speakers_render: Some("id-gone".into()),
            mic_render: Some("id-gone-too".into()),
            mic_capture: Some("id-gone-three".into()),
        };
        let a = plan(&renders, &captures, None, false, &[], &minted);
        let b = plan(&renders, &captures, None, false, &[], &MintedIds::default());
        assert_eq!(a, b);
    }

    #[test]
    fn minted_sink_narrowing_demotes() {
        let renders = [
            ep("Speakers (Realtek HD Audio)"),
            (
                "Lautsprecher (Steam Streaming Speakers)".into(),
                "id-minted-spk".into(),
            ),
        ];
        let minted = MintedIds {
            speakers_render: Some("id-minted-spk".into()),
            ..Default::default()
        };
        let p = probe(vec![("steam streaming", fmt(16_000, 1))]);
        let w = plan_with_formats(&renders, &[], None, false, &p, 2, &[], &minted);
        assert_eq!(w.loopback_render.unwrap().0, "Speakers (Realtek HD Audio)");
    }

    #[test]
    fn readiness_table() {
        let (renders, captures, minted) = minted_zoo();
        let full = plan(&renders, &captures, None, false, &[], &minted);
        assert_eq!(readiness(&full), AudioReadiness::Full);
        let renders = [ep("Altavoces (Steam Streaming Microphone)")];
        let captures = [ep("Microphone (Steam Streaming Microphone)")];
        let w = plan(&renders, &captures, None, false, &[], &MintedIds::default());
        assert_eq!(readiness(&w), AudioReadiness::AudioOnly);
        let renders = [ep("CABLE Input (VB-Audio Virtual Cable)")];
        let captures = [ep("CABLE Output (VB-Audio Virtual Cable)")];
        let w = plan(&renders, &captures, None, false, &[], &MintedIds::default());
        assert_eq!(readiness(&w), AudioReadiness::MicOnly);
        let w = plan(&[], &[], None, false, &[], &MintedIds::default());
        assert_eq!(readiness(&w), AudioReadiness::Nothing);
    }
}
