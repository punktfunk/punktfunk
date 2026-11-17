//! Pin the usbip pad's real sound card to unity gain — host half of
//! `pin_sink_volume` in pf-client-core's `pad_audio/pipewire.rs`.
//!
//! WirePlumber starts every new card at `device.routes.default-sink-volume` = 0.4, a cubed
//! mixer 40 %: 0.4³ = −23.88 dB. The default is global and re-applies on every usbip attach.
//! Capture is at the isochronous OUT, downstream of this sink, so the encoder never sees the
//! missing amplitude. Both session ends mint a card; the two stack.
//!
//! Two writes per card: the active output Route, which WirePlumber saves, mixers show and
//! re-applies on a route or profile change; and the card sink's `Props`, the gain itself.
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
    if pf_host_config::env_on("PUNKTFUNK_PAD_SINK_VOLUME") == Some(false) {
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
                match pin_pad_cards() {
                    Ok(0) => {}
                    Ok(n) => {
                        if !pinned {
                            tracing::info!(
                                pad,
                                pins = n,
                                "pad card output pinned to 0 dB (WirePlumber starts every new \
                                 card at 40% = -23.88 dB, and host+client stack)"
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

/// Pins every DualSense card's active output routes and card sinks; returns the write count,
/// `Ok(0)` while no card is in the graph. Matches the exact USB ids only, unlike the physical
/// pads `pf_client_core::pad_audio` also matches by name. PipeWire's device announce (0.3.40
/// through 1.6.0) leaves `device.vendor.id`/`device.product.id` out, so each card is bound and
/// read from its `info`.
fn pin_pad_cards() -> Result<usize> {
    use pipewire as pw;
    use pw::spa::param::ParamType;
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

    /// A bound card. `ds5` is set from its `info`, or from the announce on a PipeWire that
    /// still publishes the ids there. `routes` fills from the `Route` enumeration.
    struct Card {
        id: u32,
        device: pw::device::Device,
        _listener: pw::device::DeviceListener,
        ds5: Rc<Cell<bool>>,
        routes: Rc<RefCell<Vec<OutputRoute>>>,
    }

    let sinks: Rc<RefCell<Vec<Sink>>> = Rc::default();
    let cards: Rc<RefCell<Vec<Card>>> = Rc::default();

    let _reg_listener = session
        .registry
        .add_listener_local()
        .global({
            let (registry, sinks, cards) = (session.registry.clone(), sinks.clone(), cards.clone());
            move |g| {
                let Some(props) = g.props else { return };
                match g.type_ {
                    pw::types::ObjectType::Device => {
                        let ds5 = Rc::new(Cell::new(is_ds5(
                            props.get("device.vendor.id"),
                            props.get("device.product.id"),
                        )));
                        let Ok(device) = registry.bind::<pw::device::Device, _>(g) else {
                            return;
                        };
                        let routes: Rc<RefCell<Vec<OutputRoute>>> = Rc::default();
                        let listener = device
                            .add_listener_local()
                            .info({
                                let ds5 = ds5.clone();
                                move |info| {
                                    let Some(p) = info.props() else { return };
                                    if is_ds5(p.get("device.vendor.id"), p.get("device.product.id"))
                                    {
                                        ds5.set(true);
                                    }
                                }
                            })
                            .param({
                                let routes = routes.clone();
                                move |_seq, id, _index, _next, param| {
                                    if id != ParamType::Route {
                                        return;
                                    }
                                    if let Some(r) = param.and_then(|p| output_route(p.as_bytes()))
                                    {
                                        routes.borrow_mut().push(r);
                                    }
                                }
                            })
                            .register();
                        cards.borrow_mut().push(Card {
                            id: g.id,
                            device,
                            _listener: listener,
                            ds5,
                            routes,
                        });
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
                        // Same trap as `walk_graph` in pf-client-core's `pad_audio/pipewire.rs`.
                        // Bind for it.
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

    session.round()?; // 1: globals replay; cards and sinks get bound
    session.round()?; // 2: the binds' `info` events land: the cards' ids, audio.channels

    let mut enumerated = false;
    for c in cards.borrow().iter().filter(|c| c.ds5.get()) {
        // `Route` lists the active routes only; `EnumRoute` would list every port.
        c.device.enum_params(0, Some(ParamType::Route), 0, u32::MAX);
        enumerated = true;
    }
    if !enumerated {
        return Ok(0);
    }
    session.round()?; // 3: the DualSense cards' active routes land

    let mut pinned = 0;
    {
        let cards = cards.borrow();
        let ds5 = || cards.iter().filter(|c| c.ds5.get());
        for c in ds5() {
            for r in c.routes.borrow().iter() {
                let pod = unity_route_pod(r)?;
                let Some(pod) = pw::spa::pod::Pod::from_bytes(&pod) else {
                    continue;
                };
                c.device.set_param(ParamType::Route, 0, pod);
                pinned += 1;
            }
        }
        for s in sinks.borrow().iter() {
            // Card sink only. A host-minted pad sink publishes the DualSense identity
            // for Proton; `device.id` is what tells them apart.
            let Some(card) = s.card else { continue };
            if !ds5().any(|c| c.id == card) {
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
            s.node.set_param(ParamType::Props, 0, pod);
            pinned += 1;
        }
    }
    session.round()?; // 4: flush the set_params before the loop and its proxies drop
    Ok(pinned)
}

/// An active output route: what a Route write must echo back to land on it.
#[derive(Debug, PartialEq)]
struct OutputRoute {
    index: i32,
    device: i32,
    /// The route's own `channelVolumes` length. PipeWire ignores a write of any other length.
    channels: usize,
}

/// `None` for an input route, or one with no `channelVolumes` to match.
fn output_route(pod: &[u8]) -> Option<OutputRoute> {
    use pipewire::spa::pod::{deserialize::PodDeserializer, Value, ValueArray};
    use pipewire::spa::{sys, utils::Direction};
    let (_, Value::Object(obj)) = PodDeserializer::deserialize_any_from(pod).ok()? else {
        return None;
    };
    let (mut index, mut device, mut output, mut channels) = (None, None, false, 0);
    for p in obj.properties {
        match (p.key, p.value) {
            (sys::SPA_PARAM_ROUTE_index, Value::Int(i)) => index = Some(i),
            (sys::SPA_PARAM_ROUTE_device, Value::Int(d)) => device = Some(d),
            (sys::SPA_PARAM_ROUTE_direction, Value::Id(d)) => {
                output = d.0 == Direction::Output.as_raw()
            }
            (sys::SPA_PARAM_ROUTE_props, Value::Object(props)) => {
                channels = props
                    .properties
                    .into_iter()
                    .find_map(|p| match (p.key, p.value) {
                        (sys::SPA_PROP_channelVolumes, Value::ValueArray(ValueArray::Float(v))) => {
                            Some(v.len())
                        }
                        _ => None,
                    })
                    .unwrap_or(0)
            }
            _ => {}
        }
    }
    (output && channels > 0).then_some(OutputRoute {
        index: index?,
        device: device?,
        channels,
    })
}

/// A DualSense or DualSense Edge card, by its `device.vendor.id` and `device.product.id`.
fn is_ds5(vendor: Option<&str>, product: Option<&str>) -> bool {
    usb_id(vendor) == Some(DS5_VENDOR) && usb_id(product).is_some_and(|p| DS5_PRODUCTS.contains(&p))
}

/// A USB id as PipeWire writes it, `0x054c` or a bare `054c`. Explicit radix 16: a bare
/// `054c` under base 0 is octal.
fn usb_id(value: Option<&str>) -> Option<u32> {
    let v = value?.trim();
    v.strip_prefix("0x")
        .or_else(|| v.strip_prefix("0X"))
        .map(|h| u32::from_str_radix(h, 16))
        .unwrap_or_else(|| u32::from_str_radix(v, 16))
        .ok()
}

/// The card sink's `Props` at unity.
fn unity_volume_pod(channels: u32) -> Result<Vec<u8>> {
    use pipewire::spa::param::ParamType;
    serialize(unity_props(ParamType::Props, channels as usize))
}

/// `route` at unity. `save` makes WirePlumber store it as this card's volume, as
/// `wpctl set-volume` does.
fn unity_route_pod(route: &OutputRoute) -> Result<Vec<u8>> {
    use pipewire::spa::{self, param::ParamType, pod::Value};
    serialize(spa::pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamRoute.as_raw(),
        id: ParamType::Route.as_raw(),
        properties: vec![
            prop(spa::sys::SPA_PARAM_ROUTE_index, Value::Int(route.index)),
            prop(spa::sys::SPA_PARAM_ROUTE_device, Value::Int(route.device)),
            prop(
                spa::sys::SPA_PARAM_ROUTE_props,
                Value::Object(unity_props(ParamType::Route, route.channels)),
            ),
            prop(spa::sys::SPA_PARAM_ROUTE_save, Value::Bool(true)),
        ],
    })
}

/// Unity, unmuted, at least one `channelVolumes` entry. `id` is the param the object sits in:
/// `Props` on a node, `Route` inside a route, as PipeWire writes them.
fn unity_props(id: pipewire::spa::param::ParamType, channels: usize) -> pipewire::spa::pod::Object {
    use pipewire::spa::{self, pod::Value, pod::ValueArray};
    spa::pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamProps.as_raw(),
        id: id.as_raw(),
        properties: vec![
            prop(spa::sys::SPA_PROP_volume, Value::Float(1.0)),
            prop(
                spa::sys::SPA_PROP_channelVolumes,
                Value::ValueArray(ValueArray::Float(vec![1.0; channels.max(1)])),
            ),
            prop(spa::sys::SPA_PROP_mute, Value::Bool(false)),
        ],
    }
}

fn prop(key: u32, value: pipewire::spa::pod::Value) -> pipewire::spa::pod::Property {
    use pipewire::spa::pod::{Property, PropertyFlags};
    Property {
        key,
        flags: PropertyFlags::empty(),
        value,
    }
}

fn serialize(obj: pipewire::spa::pod::Object) -> Result<Vec<u8>> {
    use pipewire::spa::pod::{serialize::PodSerializer, Value};
    Ok(
        PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &Value::Object(obj))
            .context("serialize param pod")?
            .0
            .into_inner(),
    )
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

    /// Both spellings PipeWire uses, either pad, nothing else.
    #[test]
    fn a_dualsense_card_is_known_by_its_ids() {
        assert!(is_ds5(Some("0x054c"), Some("0x0ce6")));
        assert!(is_ds5(Some("054c"), Some("0df2")));
        assert!(is_ds5(Some(" 0X054C "), Some("0x0CE6")));
        assert!(!is_ds5(Some("0x054c"), Some("0x05c4")), "a DualShock 4");
        assert!(!is_ds5(Some("0x28de"), Some("0x0ce6")));
        assert!(!is_ds5(None, Some("0x0ce6")), "ids not announced");
        assert!(!is_ds5(Some("0x054c"), Some("junk")));
    }

    /// An empty `channelVolumes` is not "leave it"; PipeWire may apply it as-is.
    #[test]
    fn unity_pod_never_empty() {
        let bytes = unity_volume_pod(0).expect("serialize");
        assert!(!bytes.is_empty());
    }

    /// The usbip DualSense card's active route as PipeWire 1.6.8 reports it: `[Out] Direct`.
    fn box_route(direction: u32, volumes: Vec<f32>) -> Vec<u8> {
        use pipewire::spa::{self, pod::Value, pod::ValueArray, utils::Id};
        let n = volumes.len();
        let props = spa::pod::Object {
            type_: spa::utils::SpaTypes::ObjectParamProps.as_raw(),
            id: spa::param::ParamType::Route.as_raw(),
            properties: vec![
                prop(spa::sys::SPA_PROP_mute, Value::Bool(false)),
                prop(
                    spa::sys::SPA_PROP_channelVolumes,
                    Value::ValueArray(ValueArray::Float(volumes.clone())),
                ),
                prop(
                    spa::sys::SPA_PROP_channelMap,
                    Value::ValueArray(ValueArray::Id(vec![Id(3); n])),
                ),
                prop(
                    spa::sys::SPA_PROP_softVolumes,
                    Value::ValueArray(ValueArray::Float(volumes)),
                ),
                prop(spa::sys::SPA_PROP_latencyOffsetNsec, Value::Long(0)),
            ],
        };
        serialize(spa::pod::Object {
            type_: spa::utils::SpaTypes::ObjectParamRoute.as_raw(),
            id: spa::param::ParamType::Route.as_raw(),
            properties: vec![
                prop(spa::sys::SPA_PARAM_ROUTE_index, Value::Int(6)),
                prop(
                    spa::sys::SPA_PARAM_ROUTE_direction,
                    Value::Id(Id(direction)),
                ),
                prop(
                    spa::sys::SPA_PARAM_ROUTE_name,
                    Value::String("[Out] Direct".into()),
                ),
                prop(spa::sys::SPA_PARAM_ROUTE_priority, Value::Int(1000)),
                prop(
                    spa::sys::SPA_PARAM_ROUTE_info,
                    Value::Struct(vec![
                        Value::Int(1),
                        Value::String("route.hw-volume".into()),
                        Value::String("false".into()),
                    ]),
                ),
                prop(
                    spa::sys::SPA_PARAM_ROUTE_profiles,
                    Value::ValueArray(ValueArray::Int(vec![1])),
                ),
                prop(spa::sys::SPA_PARAM_ROUTE_device, Value::Int(6)),
                prop(spa::sys::SPA_PARAM_ROUTE_props, Value::Object(props)),
                prop(spa::sys::SPA_PARAM_ROUTE_profile, Value::Int(1)),
                prop(spa::sys::SPA_PARAM_ROUTE_save, Value::Bool(false)),
            ],
        })
        .expect("serialize route")
    }

    #[test]
    fn only_an_output_route_with_volumes_is_pinned() {
        use pipewire::spa::utils::Direction;
        let out = Direction::Output.as_raw();
        assert_eq!(
            output_route(&box_route(out, vec![0.064; 4])),
            Some(OutputRoute {
                index: 6,
                device: 6,
                channels: 4
            })
        );
        assert_eq!(
            output_route(&box_route(Direction::Input.as_raw(), vec![1.0; 2])),
            None,
            "the pad's mic route"
        );
        assert_eq!(output_route(&box_route(out, Vec::new())), None);
        assert_eq!(output_route(&[0u8; 8]), None);
    }

    /// The write echoes the route's index and device, matches its channel count, and saves.
    #[test]
    fn route_pod_sets_the_route_to_unity() {
        use pipewire::spa::{self, pod::deserialize::PodDeserializer, pod::Value, pod::ValueArray};
        let route = OutputRoute {
            index: 6,
            device: 6,
            channels: 4,
        };
        let bytes = unity_route_pod(&route).expect("serialize");
        let (_, Value::Object(obj)) = PodDeserializer::deserialize_any_from(&bytes).expect("parse")
        else {
            panic!("not an object pod");
        };
        assert_eq!(obj.type_, spa::utils::SpaTypes::ObjectParamRoute.as_raw());
        assert_eq!(obj.id, spa::param::ParamType::Route.as_raw());
        let get = |props: &[spa::pod::Property], key| {
            props
                .iter()
                .find(|p| p.key == key)
                .map(|p| p.value.clone())
                .unwrap_or_else(|| panic!("key {key} missing"))
        };
        let props = &obj.properties;
        assert_eq!(get(props, spa::sys::SPA_PARAM_ROUTE_index), Value::Int(6));
        assert_eq!(get(props, spa::sys::SPA_PARAM_ROUTE_device), Value::Int(6));
        assert_eq!(
            get(props, spa::sys::SPA_PARAM_ROUTE_save),
            Value::Bool(true)
        );
        let Value::Object(inner) = get(props, spa::sys::SPA_PARAM_ROUTE_props) else {
            panic!("route props is not an object");
        };
        assert_eq!(inner.type_, spa::utils::SpaTypes::ObjectParamProps.as_raw());
        let inner = &inner.properties;
        assert_eq!(get(inner, spa::sys::SPA_PROP_volume), Value::Float(1.0));
        assert_eq!(get(inner, spa::sys::SPA_PROP_mute), Value::Bool(false));
        assert_eq!(
            get(inner, spa::sys::SPA_PROP_channelVolumes),
            Value::ValueArray(ValueArray::Float(vec![1.0; 4]))
        );
    }
}
