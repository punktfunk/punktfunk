//! Pin the usbip pad's real sound card to unity gain — host half of
//! `pf_client_core::pad_audio::pin_sink_volume`.
//!
//! WirePlumber starts every new card at `device.routes.default-sink-volume` = 0.4, a cubed
//! mixer 40 %: 0.4³ = −23.88 dB. The default is global and re-applies on every usbip attach.
//! Capture is at the isochronous OUT, downstream of this sink, so the encoder never sees the
//! missing amplitude. Both session ends mint a card; the two stack.
//!
//! Not restored: this overrides a default nobody chose. Failures cost loudness, not audio.
//! `PUNKTFUNK_PAD_SINK_VOLUME=0` skips.

use anyhow::{Context, Result};
use std::time::Duration;

const DS5_VENDOR: u32 = 0x054c;
const DS5_PRODUCTS: [u32; 2] = [0x0ce6, 0x0df2];

/// 15 × 1 s: the USB device is live before its sink, and an early pin is overwritten
/// by WirePlumber's default. A missing card is a pad with no sink; give up quietly.
const ATTEMPTS: u32 = 15;
const INTERVAL: Duration = Duration::from_secs(1);

/// Detached: the caller is the capture-thread open path, and waiting there drops pad audio.
pub fn spawn_pin(pad: u8) {
    if matches!(
        std::env::var("PUNKTFUNK_PAD_SINK_VOLUME").as_deref(),
        Ok("0" | "false" | "off" | "no")
    ) {
        return;
    }
    if let Err(e) = std::thread::Builder::new()
        .name(format!("punktfunk1-padvol{pad}"))
        .spawn(move || {
            // Connect errors retry like an absent card: attach is when the graph is busy.
            // Only the last failure is reported. The whole window runs: another pad's card
            // can be the first one found, with this pad's own still to land.
            let mut last_err = None;
            let mut pinned = false;
            for _ in 0..ATTEMPTS {
                match pin_pad_sinks() {
                    Ok(0) => {}
                    Ok(n) => {
                        if !pinned {
                            tracing::info!(
                                pad,
                                sinks = n,
                                "pad card sink pinned to 0 dB (WirePlumber starts every new card \
                                 at 40% = -23.88 dB, and host+client stack)"
                            );
                        }
                        pinned = true;
                    }
                    Err(e) => last_err = Some(format!("{e:#}")),
                }
                std::thread::sleep(INTERVAL);
            }
            if pinned {
                return;
            }
            tracing::debug!(
                pad,
                error = last_err.unwrap_or_else(|| "no DualSense card sink in the graph".into()),
                "pad sink volume not pinned — pad audio may be quiet if this box attenuates it"
            );
        })
    {
        tracing::debug!(pad, error = %e, "pad sink volume thread not spawned");
    }
}

/// `Ok(0)` means the card is not in the graph yet. Matches the exact USB ids only: the
/// usbip-minted pad always announces `device.vendor.id`/`device.product.id`, unlike the
/// physical pads `pf_client_core::pad_audio` also matches by name.
fn pin_pad_sinks() -> Result<usize> {
    use pipewire as pw;
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    let session = super::pw_oneshot::OneShot::connect("pad-volume", super::pw_oneshot::TIMEOUT)?;

    struct Sink {
        node: pw::node::Node,
        _listener: pw::node::NodeListener,
        /// Announce `device.id`. `None` = no card (a host-minted pad sink).
        card: Option<u32>,
        /// From the bound node's `info`, not the announce. Zero until that lands.
        channels: Rc<Cell<u32>>,
    }

    let sinks: Rc<RefCell<Vec<Sink>>> = Rc::default();
    let ds5_cards: Rc<RefCell<Vec<u32>>> = Rc::default();

    let _reg_listener = session
        .registry
        .add_listener_local()
        .global({
            let (registry, sinks, ds5_cards) =
                (session.registry.clone(), sinks.clone(), ds5_cards.clone());
            move |g| {
                let Some(props) = g.props else { return };
                let usb_id = |k: &str| {
                    props.get(k).and_then(|v| {
                        let v = v.trim();
                        // Explicit radix 16: a bare `054c` under base 0 is octal.
                        v.strip_prefix("0x")
                            .or_else(|| v.strip_prefix("0X"))
                            .map(|h| u32::from_str_radix(h, 16))
                            .unwrap_or_else(|| u32::from_str_radix(v, 16))
                            .ok()
                    })
                };
                match g.type_ {
                    // Cards publish vendor/product on announce; no bind needed.
                    pw::types::ObjectType::Device => {
                        let vendor = usb_id("device.vendor.id");
                        let product = usb_id("device.product.id");
                        if vendor == Some(DS5_VENDOR)
                            && product.is_some_and(|p| DS5_PRODUCTS.contains(&p))
                        {
                            ds5_cards.borrow_mut().push(g.id);
                        }
                    }
                    pw::types::ObjectType::Node => {
                        if !props
                            .get("media.class")
                            .is_some_and(|c| c.starts_with("Audio/Sink"))
                        {
                            return;
                        }
                        let Ok(node) = registry.bind::<pw::node::Node, _>(g) else {
                            return;
                        };
                        // `audio.channels` is not in the announce subset (reads as 0).
                        // Same trap as `pf_client_core::pad_audio::walk_graph`. Bind for it.
                        let channels = Rc::new(Cell::new(0u32));
                        let listener = node
                            .add_listener_local()
                            .info({
                                let channels = channels.clone();
                                move |info| {
                                    let Some(p) = info.props() else { return };
                                    if let Some(c) =
                                        p.get("audio.channels").and_then(|v| v.parse().ok())
                                    {
                                        channels.set(c);
                                    }
                                }
                            })
                            .register();
                        sinks.borrow_mut().push(Sink {
                            node,
                            _listener: listener,
                            card: props.get("device.id").and_then(|v| v.parse().ok()),
                            channels,
                        });
                    }
                    _ => {}
                }
            }
        })
        .register();

    session.round()?; // 1: globals replay; sinks get bound
    session.round()?; // 2: the binds' `info` events land, carrying audio.channels

    let mut pinned = 0;
    {
        let cards = ds5_cards.borrow();
        for s in sinks.borrow().iter() {
            // Card sink only. A host-minted pad sink publishes the DualSense identity
            // for Proton; `device.id` is what tells them apart.
            let Some(card) = s.card else { continue };
            if !cards.contains(&card) {
                continue;
            }
            let channels = s.channels.get();
            if channels == 0 {
                continue;
            }
            let pod = unity_volume_pod(channels)?;
            let Some(pod) = pw::spa::pod::Pod::from_bytes(&pod) else {
                continue;
            };
            s.node.set_param(pw::spa::param::ParamType::Props, 0, pod);
            pinned += 1;
        }
    }
    session.round()?; // 3: flush the set_params before the loop and its proxies drop
    Ok(pinned)
}

fn unity_volume_pod(channels: u32) -> Result<Vec<u8>> {
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
    .context("serialize Props pod")?
    .0
    .into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// PipeWire ignores a `channelVolumes` length that does not match the port count —
    /// the pin would look like it ran and do nothing.
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

    /// An empty `channelVolumes` is not "leave it"; PipeWire may apply it as-is.
    #[test]
    fn unity_pod_never_empty() {
        let bytes = unity_volume_pod(0).expect("serialize");
        assert!(!bytes.is_empty());
    }
}
