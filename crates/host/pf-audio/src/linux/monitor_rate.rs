//! The monitored node's own rate, from the PipeWire registry — not the capture stream's.
//!
//! In `PUNKTFUNK_STREAM_SINK=0` the host records someone else's sink through PipeWire's
//! resampler, which reports whatever rate we asked for. `AudioCapturer::sample_rate` is
//! therefore the request, not the device. WirePlumber's `default` metadata names the
//! elected sink; only a bind's negotiated `Format` names its rate (the announce props
//! omit it — same trap as `audio.channels` in `pf_client_core::pad_audio::walk_graph`).
//!
//! Every failure is [`super::super::CaptureRate::Unknown`]: a missing node, unset key,
//! unnegotiated format, or timeout all decline. Over-claiming advertises 96 kHz of
//! interpolated 48 kHz; under-claiming is Opus. Nothing here guesses a rate.
//! See `design/hi-res-audio.md`.

use anyhow::{anyhow, bail, Result};
use std::time::Duration;

/// WirePlumber's elected default sink — the node an untargeted `stream.capture.sink=true`
/// stream is linked to. Not [`super::stream_sink`]'s `default.configured.audio.sink`: that
/// is the user's preference, unset when they never chose, and can name a gone node.
const DEFAULT_SINK_KEY: &str = "default.audio.sink";

/// Handshake budget. Shorter than [`super::pw_oneshot::TIMEOUT`]: a stall here delays
/// `Welcome`; giving up only declines to Opus.
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// `{"name":"alsa_output.…"}` from WirePlumber / [`super::stream_sink`]. Hand-parsed: a
/// miss is `None` → decline, never a wrong rate. Node names have no quotes or backslashes.
fn sink_name_from_json(value: &str) -> Option<String> {
    // Every `"name"` followed by `:`, not the first match: another member's value can
    // contain the word.
    for (at, key) in value.match_indices("\"name\"") {
        let Some(rest) = value[at + key.len()..].trim_start().strip_prefix(':') else {
            continue;
        };
        let Some(quoted) = rest.trim_start().strip_prefix('"') else {
            return None; // `null`, a number, an object — anything but a name.
        };
        let name = quoted.split_once('"')?.0;
        return (!name.is_empty()).then(|| name.to_string());
    }
    None
}

/// `Err` is a decline, not a fault.
pub(super) fn monitored_sink_rate() -> Result<u32> {
    use pipewire as pw;
    use pw::spa::param::audio::AudioInfoRaw;
    use pw::spa::param::ParamType;
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    let session = super::pw_oneshot::OneShot::connect("monitor-rate", PROBE_TIMEOUT)?;
    // Bound eagerly: a registry global cannot be bound later, and which sink is elected is
    // unknown until the metadata replay.
    let sinks: Rc<RefCell<Vec<(String, pw::node::Node, pw::node::NodeListener)>>> = Rc::default();
    let rate: Rc<Cell<Option<u32>>> = Rc::default();
    let _registry_listener = session
        .registry
        .add_listener_local()
        .global({
            let (registry, sinks, rate) = (session.registry.clone(), sinks.clone(), rate.clone());
            move |global| {
                // Rate is not in the announce props — binding is what fetches it.
                // `starts_with("Audio/Sink")` includes `Audio/Sink/Internal`.
                let Some(props) = global.props else { return };
                if global.type_ != pw::types::ObjectType::Node
                    || !props
                        .get("media.class")
                        .is_some_and(|c| c.starts_with("Audio/Sink"))
                {
                    return;
                }
                let Some(name) = props.get("node.name") else {
                    return;
                };
                let Ok(node) = registry.bind::<pw::node::Node, _>(global) else {
                    return;
                };
                let listener = node
                    .add_listener_local()
                    .param({
                        let rate = rate.clone();
                        move |_seq, id, _index, _next, param| {
                            if id != ParamType::Format {
                                return;
                            }
                            let Some(param) = param else { return };
                            let mut info = AudioInfoRaw::default();
                            // 0 or a non-audio/raw pod (IEC958/DSD) is not a rate.
                            // `parse` can leave a partially-filled struct; do not trust it.
                            if info.parse(param).is_ok() && info.rate() != 0 && rate.get().is_none()
                            {
                                rate.set(Some(info.rate()));
                            }
                        }
                    })
                    .register();
                sinks.borrow_mut().push((name.to_string(), node, listener));
            }
        })
        .register();

    let (_metadata, defaults) = session.default_metadata()?;
    let Some(elected) = defaults
        .get(DEFAULT_SINK_KEY)
        .and_then(|v| sink_name_from_json(v))
    else {
        bail!(
            "'{DEFAULT_SINK_KEY}' is unset — no sink has been elected, so there is nothing for a \
             monitor capture to follow"
        );
    };
    {
        let sinks = sinks.borrow();
        let Some((_, node, _)) = sinks.iter().find(|(n, ..)| *n == elected) else {
            bail!("the elected default sink '{elected}' is not in the graph");
        };
        // `Format` (configured), never `EnumFormat` (capability). On an ALSA adapter this is
        // the device side; the monitor tap is the graph side. Low is a safe decline; high
        // would over-claim. The monitor port's own format is the extra hop this does not take.
        node.enum_params(0, Some(ParamType::Format), 0, 1);
    }
    session.round()?;
    // No negotiated format means the sink is idle/suspended — the rate it will pick is not a
    // fact yet. Decline rather than predict.
    rate.get().ok_or_else(|| {
        anyhow!("the elected default sink has no negotiated format (it is idle/suspended)")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Compact JSON from WirePlumber and [`super::super::stream_sink`], plus a spaced form.
    /// Formatting is not in the protocol.
    #[test]
    fn reads_the_node_name_wireplumber_writes() {
        assert_eq!(
            sink_name_from_json(r#"{"name":"alsa_output.pci-0000_00_1f.3.analog-stereo"}"#),
            Some("alsa_output.pci-0000_00_1f.3.analog-stereo".into())
        );
        assert_eq!(
            sink_name_from_json(r#"{ "name": "punktfunk-speaker-4242-0" }"#),
            Some("punktfunk-speaker-4242-0".into())
        );
    }

    /// `None` is a decline. A plausible parse would look up a node and believe its rate.
    #[test]
    fn nothing_parseable_is_never_guessed() {
        for v in [
            "",
            "{}",
            r#"{"name":}"#,
            r#"{"name":""}"#,
            r#"{"name":null}"#,
            "alsa_output.pci-0000_00_1f.3.analog-stereo",
            r#"{"nickname":"alsa_output.x"}"#,
        ] {
            assert_eq!(sink_name_from_json(v), None, "{v:?} must not parse");
        }
    }

    #[test]
    fn only_the_name_key_is_read() {
        assert_eq!(
            sink_name_from_json(r#"{"other":"name","name":"alsa_output.x"}"#),
            Some("alsa_output.x".into())
        );
    }
}
