//! Per-pad DualSense audio topology (Linux). Mints the PipeWire node graph a
//! physically connected DualSense presents, so a game that renders voice-coil
//! haptics or pad-speaker audio finds "the controller's audio device" and
//! plays into us. We own the nodes: `process()` is the capture, mixed into
//! one 4-ch F32 48 kHz quad (ch0/1 speaker/headphone, ch2/3 voice coils) that
//! feeds the 0xD1 lanes (the host's `native/pad_audio.rs`).
//!
//! Three nodes match the UCM split. The public 4-ch surface is positioned
//! FL/FR/RL/RR (`SpeakerHaptic__sink`); a positioned writer on an AUX node is
//! remixed and the coil pair is discarded. The hidden AUX parent
//! (`Audio/Sink/Internal`) is GE-Proton's `pipewire:NODE=` target. The public
//! mono `Speaker__sink` is the substring GE binds the controller-effect stream
//! to. Vendor/product ids carry the `0x` prefix `strtoul(_, 0)` needs.
//! Description is `"Wireless Controller"` so wine's `wcsstr` on FriendlyName
//! matches. `priority.session` stays low — these must never win default-sink
//! election. Do not set `api.alsa.split.position` (WirePlumber's split trigger)
//! or `api.alsa.path` (wine's raw-ALSA leg needs a real card; we have none).
//!
//! Env: `PUNKTFUNK_PAD_SINK_NAME` / `_DESC` / `_SPLIT_NAME` / `_PARENT_CLASS`.
//! Channel map: [`PadNode::channel_map`].

use anyhow::{anyhow, Context, Result};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError};
use std::thread;
use std::time::Duration;

struct Terminate;

/// Hardware quad every node mixes onto.
const PAD_CHANNELS: u32 = 4;

/// How many pads get a sink (`PUNKTFUNK_PAD_AUDIO_SLOTS`). Default 4: a PipeWire
/// stream node is cheap; the Windows mint defaults to 1.
pub fn pad_audio_slots() -> u8 {
    std::env::var("PUNKTFUNK_PAD_AUDIO_SLOTS")
        .ok()
        .and_then(|s| s.parse::<u8>().ok())
        .unwrap_or(4)
        .clamp(1, 4)
}

/// Whether a PipeWire daemon is plausibly reachable. Stat, not connect: the
/// handshake runs per-Hello and must not block. `PIPEWIRE_REMOTE` names a
/// non-default socket; a wrong value fails at spawn, pad kept.
pub fn pipewire_reachable() -> bool {
    if std::env::var_os("PIPEWIRE_REMOTE").is_some() {
        return true;
    }
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(|dir| std::path::Path::new(&dir).join("pipewire-0").exists())
        .unwrap_or(false)
}

/// Colon-separated display MAC of pad `pad` (a DualSense Edge when `edge`).
/// [`ds_pairing_reply`] bytes 1..7 are LSB-first (`hid-playstation` `%pMR`
/// uniq), so the display form reverses them. Used only by the `{mac}`
/// identity-template placeholder.
///
/// [`ds_pairing_reply`]: pf_inject::dualsense_proto::ds_pairing_reply
fn pad_mac(pad: u8, edge: bool) -> String {
    use pf_inject::dualsense_proto::{ds_pairing_reply, DEVTYPE_DUALSENSE, DEVTYPE_DUALSENSE_EDGE};
    let device_type = if edge {
        DEVTYPE_DUALSENSE_EDGE
    } else {
        DEVTYPE_DUALSENSE
    };
    let reply = ds_pairing_reply(device_type, pad);
    let m = &reply[1..7];
    format!(
        "{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
        m[5], m[4], m[3], m[2], m[1], m[0]
    )
}

/// Expand `{pad}` / `{mac}` in an identity template. Pass the MAC as the
/// surrounding string wants it: colon display for proplist, bare hex for
/// ALSA-style names (udev serials have no colons).
fn expand(template: &str, pad: u8, mac: &str) -> String {
    template
        .replace("{pad}", &pad.to_string())
        .replace("{mac}", mac)
}

struct PadSinkIdentity {
    /// Public mono speaker sink. GE-Proton's `is_dualsense_speaker_sink` binds
    /// the controller-effect stream here; the rest of the identity hangs off it.
    speaker_name: String,
    /// Public positioned-quad sink (UCM `SpeakerHaptic`).
    haptic_name: String,
    /// Hidden AUX-quad parent: capture point and GE's `pipewire:NODE=` target.
    parent_name: String,
    description: String,
    serial: String,
    product_id: &'static str,
    product_name: &'static str,
    card_name: &'static str,
    long_card_name: String,
    components: &'static str,
    /// `api.alsa.split.name` GE opens as `pipewire:NODE=` for haptics. Empty
    /// omits the key. See [`split_target`].
    split_name: String,
}

/// GE-Proton reads `api.alsa.split.name` off the haptic sink and opens that
/// node as `pipewire:NODE=<name>` with `aux_channels=1`. The key must name the
/// hidden 4-ch parent: four channels at the public 1-ch `Speaker__sink` lose
/// the coil pair. The parent's own copy is self-referential, as on hardware.
/// `PUNKTFUNK_PAD_SINK_SPLIT_NAME`: `0`/`false`/`off` drops the key (GE takes
/// Pulse, whose AUX0..AUX3 map already matches); any other value overrides.
fn split_target(parent_name: &str) -> String {
    resolve_split_target(
        parent_name,
        std::env::var("PUNKTFUNK_PAD_SINK_SPLIT_NAME").ok(),
    )
}

fn resolve_split_target(parent_name: &str, override_var: Option<String>) -> String {
    match override_var.as_deref().map(str::trim) {
        Some("0" | "false" | "off" | "no") => String::new(),
        Some(v) if !v.is_empty() => v.to_string(),
        _ => parent_name.to_string(),
    }
}

impl PadSinkIdentity {
    fn new(pad: u8, edge: bool) -> PadSinkIdentity {
        let mac = pad_mac(pad, edge);
        let mac_bare: String = mac.chars().filter(|c| *c != ':').collect();
        // USB `iProduct` verbatim, model word included. GE's fallback is two
        // substrings (`alsa_output.usb-Sony_Interactive_Entertainment_` AND
        // `Wireless_Controller`); dropping DualSense would still pass, but no
        // real pad's ALSA card does.
        let (usb_product, product_id, product_name, card_name, components) = if edge {
            (
                "DualSense_Edge_Wireless_Controller",
                "0x0df2",
                "DualSense Edge Wireless Controller",
                "DualSense Edge Wireless Controller",
                "USB054c:0df2",
            )
        } else {
            (
                "DualSense_Wireless_Controller",
                "0x0ce6",
                "DualSense wireless controller (PS5)",
                "DualSense Wireless Controller",
                "USB054c:0ce6",
            )
        };
        // udev `ID_SERIAL` is manufacturer_product[_serial]. A DualSense has no
        // USB iSerialNumber, so ALSA disambiguates with the trailing card index.
        // Pad index goes there; do not invent a MAC infix.
        let serial = format!("Sony_Interactive_Entertainment_{usb_product}");
        let card_index = format!("{pad:02}");
        // `…Speaker__sink` is load-bearing: GE's `is_dualsense_speaker_sink()` is
        // a substring test. `SpeakerHaptic__sink` must not contain it (the
        // `Haptic` infix is what keeps the mono stream off the quad).
        let speaker_name = match std::env::var("PUNKTFUNK_PAD_SINK_NAME") {
            Ok(t) if !t.trim().is_empty() => expand(&t, pad, &mac_bare),
            _ => format!("alsa_output.usb-{serial}-{card_index}.HiFi__Speaker__sink"),
        };
        let haptic_name =
            format!("alsa_output.usb-{serial}-{card_index}.HiFi__SpeakerHaptic__sink");
        let parent_name = format!("alsa_output.hw_punktfunkpad{pad}_0");
        let description = match std::env::var("PUNKTFUNK_PAD_SINK_DESC") {
            Ok(t) if !t.trim().is_empty() => expand(&t, pad, &mac),
            _ => "Wireless Controller".to_string(),
        };
        let split_name = split_target(&parent_name);
        PadSinkIdentity {
            long_card_name: format!(
                "Sony Interactive Entertainment {card_name} at usb-punktfunk-pad{pad}, full speed"
            ),
            speaker_name,
            haptic_name,
            parent_name,
            description,
            serial,
            product_id,
            product_name,
            card_name,
            components,
            split_name,
        }
    }
}

/// Which node a buffer arrived on. Discriminant is the mixer's contribution bit.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PadNode {
    /// Hidden AUX parent — GE's haptic leg and Pro-Audio-shaped writers.
    Parent = 0,
    /// Public positioned quad (UCM `SpeakerHaptic`).
    Haptic = 1,
    Speaker = 2,
}

impl PadNode {
    /// Source-channel → hardware-quad-channel map.
    ///
    /// Both quads are identity (UCM `HeadphonesHaptic`: 0,1,2,3). Do not fold
    /// FL+FR into ch1 the way `SpeakerHaptic` does — that feed is a physical
    /// mono speaker; we ship the stereo pair onward. Mono speaker maps to
    /// **ch1** (UCM `Speaker` `Channel0 1`). Landing it on ch0 puts
    /// controller-effect audio in headphone L and leaves the speaker silent.
    fn channel_map(self) -> &'static [usize] {
        match self {
            PadNode::Parent | PadNode::Haptic => &[0, 1, 2, 3],
            PadNode::Speaker => &[1],
        }
    }

    fn src_channels(self) -> usize {
        match self {
            PadNode::Parent | PadNode::Haptic => 4,
            PadNode::Speaker => 1,
        }
    }
}

/// Sums the three nodes into one hardware quad.
///
/// A real pad's public sinks are SplitPCM views the kernel sums. We have three
/// independent nodes, and GE drives two at once, so emitting each buffer
/// straight would interleave them. Alignment is by contribution round, not
/// timestamp: a node that contributes twice closes the window. Cost is one
/// quantum of buffering (~5 ms at the usual 240-frame quantum).
struct Mixer {
    buf: Vec<f32>,
    /// Frames accumulated (longest contribution this round).
    frames: usize,
    /// Bitmask of [`PadNode`]s that have contributed to the open window.
    contributed: u8,
    tx: std::sync::mpsc::SyncSender<Vec<f32>>,
    /// Full channel = 0xD1 encode stalled. Count and warn, power-of-two
    /// throttled (this runs at graph quantum).
    dropped: u64,
}

impl Mixer {
    fn new(tx: std::sync::mpsc::SyncSender<Vec<f32>>) -> Mixer {
        Mixer {
            buf: Vec::new(),
            frames: 0,
            contributed: 0,
            tx,
            dropped: 0,
        }
    }

    fn add(&mut self, node: PadNode, samples: &[f32]) {
        let src = node.src_channels();
        let bit = 1u8 << (node as u8);
        // This node is on its next round — close the window peers already filled.
        if self.contributed & bit != 0 {
            self.flush();
        }
        self.contributed |= bit;
        let quad = PAD_CHANNELS as usize;
        let n = samples.len() / src;
        if self.buf.len() < n * quad {
            self.buf.resize(n * quad, 0.0);
        }
        self.frames = self.frames.max(n);
        for (f, frame) in samples.chunks_exact(src).enumerate() {
            for (s, &out) in node.channel_map().iter().enumerate() {
                self.buf[f * quad + out] += frame[s];
            }
        }
    }

    fn flush(&mut self) {
        self.contributed = 0;
        if self.frames == 0 {
            return;
        }
        let used = self.frames * PAD_CHANNELS as usize;
        let out = self.buf[..used].to_vec();
        self.buf[..used].fill(0.0);
        self.frames = 0;
        if self.tx.try_send(out).is_err() {
            self.dropped += 1;
            if self.dropped.is_power_of_two() {
                tracing::warn!(
                    dropped = self.dropped,
                    "pad-audio encode thread not keeping up — captured pad audio dropped \
                     (haptics will click)"
                );
            }
        }
    }
}

struct PadUd {
    mix: Rc<RefCell<Mixer>>,
    node: PadNode,
}

/// Live per-pad node graph and its capture. [`AudioCapturer`] contract: empty
/// chunk = quiet pad (keep me), `Err` = dead loop thread (reopen me). Drop
/// sends Terminate promptly — a wedged PipeWire link head-blocks the daemon.
pub struct PadSinkCapturer {
    chunks: Receiver<Vec<f32>>,
    quit: pipewire::channel::Sender<Terminate>,
    /// Public mono speaker sink — the identity a title has to match.
    pub node_name: String,
    /// Public positioned-quad sink (UCM `SpeakerHaptic`).
    pub haptic_name: String,
    /// Hidden AUX parent / `api.alsa.split.name`. Empty when the key is omitted.
    pub split_name: String,
}

impl PadSinkCapturer {
    /// Mint the node graph for the pad at OS slot `pad` (`edge` = DualSense Edge)
    /// and start capturing. The slot, not the client's wire numbering: these node
    /// names and the pad MAC are host-wide, and two sessions both number from zero.
    /// Fails if PipeWire is unreachable; the caller owns reopen-with-backoff.
    pub fn open(pad: u8, edge: bool) -> Result<PadSinkCapturer> {
        let identity = PadSinkIdentity::new(pad, edge);
        let node_name = identity.speaker_name.clone();
        let haptic_name = identity.haptic_name.clone();
        let parent_name = identity.parent_name.clone();
        let split_name = identity.split_name.clone();
        let (tx, rx) = sync_channel::<Vec<f32>>(64);
        let (quit_tx, quit_rx) = pipewire::channel::channel::<Terminate>();
        // Bring-up handshake: a missing PipeWire must fail `open`, not leave a
        // zombie thread. The caller owns backoff.
        let (ready_tx, ready_rx) = sync_channel::<Result<()>>(1);
        thread::Builder::new()
            .name(format!("punktfunk-pw-pad{pad}"))
            .spawn(move || {
                if let Err(e) = pad_sink_thread(tx, quit_rx, identity, ready_tx) {
                    tracing::warn!(pad, error = %format!("{e:#}"), "pipewire pad-sink thread failed");
                }
            })
            .context("spawn pipewire pad-sink thread")?;
        match ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(e),
            Err(_) => {
                // The thread may still come up; it must not outlive this error with live sinks.
                let _ = quit_tx.send(Terminate);
                return Err(anyhow!("pipewire pad-sink init timed out"));
            }
        }
        let split_log = if split_name.is_empty() {
            "(suppressed)"
        } else {
            split_name.as_str()
        };
        tracing::info!(
            pad,
            edge,
            speaker_sink = %node_name,
            haptic_sink = %haptic_name,
            parent = %parent_name,
            split_name = %split_log,
            "pad-audio nodes minted (real-pad split: mono Speaker__sink + positioned \
             SpeakerHaptic__sink + hidden AUX parent; ch0/1 speaker, ch2/3 coils)"
        );
        Ok(PadSinkCapturer {
            chunks: rx,
            quit: quit_tx,
            node_name,
            haptic_name,
            split_name,
        })
    }
}

impl Drop for PadSinkCapturer {
    fn drop(&mut self) {
        // A failed send means the loop thread already exited — nothing to tear down.
        let _ = self.quit.send(Terminate);
    }
}

impl crate::AudioCapturer for PadSinkCapturer {
    fn next_chunk(&mut self) -> Result<Vec<f32>> {
        match self.chunks.recv_timeout(Duration::from_secs(5)) {
            Ok(c) => Ok(c),
            // Quiet pad is not a failure: keep the capturer; silence gate stays closed.
            Err(RecvTimeoutError::Timeout) => Ok(Vec::new()),
            Err(RecvTimeoutError::Disconnected) => Err(anyhow!("pipewire pad-sink thread ended")),
        }
    }

    fn channels(&self) -> u32 {
        PAD_CHANNELS
    }
}

/// SPA positions for the parent's AUX quad (`SPA_AUDIO_CHANNEL_START_Aux` =
/// 0x1000), not a positioned layout. GE's haptic path opens this node with
/// `aux_channels=1` and Pulse forces `AUX0..3`. Aux carries no spatial meaning,
/// so the graph does not remix: writers land by index.
fn aux_positions() -> [u32; 64] {
    const AUX0: u32 = 0x1000;
    let mut pos = [0u32; 64];
    pos[..4].copy_from_slice(&[AUX0, AUX0 + 1, AUX0 + 2, AUX0 + 3]);
    pos
}

/// SPA positions for the public quad: FL, FR, RL, RR — the only 4-ch surface a
/// real pad publishes. Accepting it unfolded is why this node exists.
fn positioned_quad() -> [u32; 64] {
    let mut pos = [0u32; 64];
    // spa_audio_channel: FL = 3, FR = 4, RL = 12, RR = 13.
    pos[..4].copy_from_slice(&[3, 4, 12, 13]);
    pos
}

/// SPA positions for the public mono sink (`SPA_AUDIO_CHANNEL_MONO` = 2).
fn mono_position() -> [u32; 64] {
    let mut pos = [0u32; 64];
    pos[0] = 2;
    pos
}

fn format_pod(channels: u32, positions: [u32; 64]) -> Result<Vec<u8>> {
    use pipewire as pw;
    use pw::spa::param::audio::{AudioFormat, AudioInfoRaw};
    let mut info = AudioInfoRaw::new();
    info.set_format(AudioFormat::F32LE);
    info.set_rate(crate::SAMPLE_RATE);
    info.set_channels(channels);
    info.set_position(positions);
    let obj = pw::spa::pod::Object {
        type_: pw::spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: pw::spa::param::ParamType::EnumFormat.as_raw(),
        properties: info.into(),
    };
    Ok(pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(obj),
    )
    .context("serialize pad-sink format pod")?
    .0
    .into_inner())
}

/// Shared `process()` body. A macro so the PipeWire stream type never has to
/// be named (it is only reachable through the builder's inference).
macro_rules! pad_process {
    () => {
        |stream, ud: &mut PadUd| {
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let Some(mut buffer) = stream.dequeue_buffer() else {
                    return;
                };
                let datas = buffer.datas_mut();
                if datas.is_empty() {
                    return;
                }
                let d = &mut datas[0];
                let (offset, size) = {
                    let c = d.chunk();
                    (c.offset() as usize, c.size() as usize)
                };
                let Some(buf) = d.data() else { return };
                if offset > buf.len() {
                    return;
                }
                let region = &buf[offset..(offset + size).min(buf.len())];
                let n = region.len() / 4;
                let mut samples = Vec::with_capacity(n);
                for i in 0..n {
                    samples.push(f32::from_le_bytes([
                        region[i * 4],
                        region[i * 4 + 1],
                        region[i * 4 + 2],
                        region[i * 4 + 3],
                    ]));
                }
                ud.mix.borrow_mut().add(ud.node, &samples);
            }));
            if outcome.is_err() {
                tracing::error!("panic in pipewire pad-sink callback — chunk dropped");
            }
        }
    };
}

/// `!Send` MainLoop/Stream thread: mint the three nodes, hand mixed chunks
/// over, run until Terminate or daemon death.
fn pad_sink_thread(
    tx: std::sync::mpsc::SyncSender<Vec<f32>>,
    quit_rx: pipewire::channel::Receiver<Terminate>,
    identity: PadSinkIdentity,
    ready: std::sync::mpsc::SyncSender<Result<()>>,
) -> Result<()> {
    use pipewire as pw;
    use pw::{properties::properties, spa};
    use spa::param::audio::AudioInfoRaw;
    use spa::pod::Pod;

    let result = (|| -> Result<()> {
        pf_capture::pwinit::ensure_init();
        let mainloop = pw::main_loop::MainLoopRc::new(None).context("pw pad-sink MainLoop")?;
        let context =
            pw::context::ContextRc::new(&mainloop, None).context("pw pad-sink Context")?;
        let core = context
            .connect_rc(None)
            .context("pw pad-sink connect (is PipeWire running in this session?)")?;

        let _quit_guard = quit_rx.attach(mainloop.loop_(), {
            let mainloop = mainloop.clone();
            move |_| mainloop.quit()
        });

        // Daemon death ends this thread, the chunk channel disconnects,
        // `next_chunk` errors, the per-pad streamer reopens with backoff.
        let _core_listener = core
            .add_listener_local()
            .error({
                let mainloop = mainloop.clone();
                move |id, _seq, res, message| {
                    tracing::warn!(id, res, message, "pipewire core error — pad sink ends");
                    mainloop.quit();
                }
            })
            .register();

        let mix = Rc::new(RefCell::new(Mixer::new(tx)));

        // Same card identity on every node: they are three views of one USB
        // card, and matchers read these off whichever node they hold.
        let base_props = |class: &str, name: &str, channels: &str, position: &str| {
            let mut p = properties! {
                *pw::keys::MEDIA_TYPE   => "Audio",
                // One Opus-haptics frame (~5 ms) per quantum. Haptics are felt
                // latency; bursty delivery rides into the client's jitter buffer.
                *pw::keys::NODE_LATENCY => "240/48000",
                // Must never win WirePlumber's default election. Games reach
                // these nodes by identity; nothing auto-routes here.
                "priority.session"      => "50",
                "device.bus"            => "usb",
                "device.vendor.id"      => "0x054c",
                "device.vendor.name"    => "Sony Corp.",
                "device.form_factor"    => "gamepad",
                "device.icon_name"      => "audio-card-analog",
                "api.alsa.pcm.stream"   => "playback",
                "api.alsa.open.ucm"     => "true",
                "alsa.driver_name"      => "snd_usb_audio",
                "node.virtual"          => "false",
                // Parent's raw layout, informational. Do not set
                // `api.alsa.split.position` — that is WirePlumber's split trigger.
                "api.alsa.split.hw-position" => "[AUX0,AUX1,AUX2,AUX3]",
            };
            p.insert(*pw::keys::MEDIA_CLASS, class);
            p.insert(*pw::keys::NODE_NAME, name);
            p.insert(*pw::keys::NODE_DESCRIPTION, identity.description.as_str());
            p.insert("audio.channels", channels);
            p.insert("audio.position", position);
            p.insert("device.serial", identity.serial.as_str());
            p.insert("device.product.id", identity.product_id);
            p.insert("device.product.name", identity.product_name);
            p.insert("device.description", identity.product_name);
            p.insert("alsa.card_name", identity.card_name);
            p.insert("alsa.long_card_name", identity.long_card_name.as_str());
            p.insert("api.alsa.card.name", identity.card_name);
            p.insert("api.alsa.card.longname", identity.long_card_name.as_str());
            p.insert("alsa.components", identity.components);
            p.insert("alsa.id", "Controller");
            // GE's preferred haptic leg; see `split_target`. Omit (do not insert
            // "") when off, so `pa_proplist_gets` misses rather than returning "".
            if !identity.split_name.is_empty() {
                p.insert("api.alsa.split.name", identity.split_name.as_str());
            }
            p
        };

        // Hidden AUX parent (`Audio/Sink/Internal`): off pactl/pulse lists,
        // still openable by name. `PUNKTFUNK_PAD_SINK_PARENT_CLASS` flips it if
        // a session manager refuses Internal clients.
        let parent_class = std::env::var("PUNKTFUNK_PAD_SINK_PARENT_CLASS")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "Audio/Sink/Internal".to_string());
        let mut parent_props = base_props(
            &parent_class,
            &identity.parent_name,
            "4",
            "AUX0,AUX1,AUX2,AUX3",
        );
        parent_props.insert("api.alsa.split.parent", "true");
        parent_props.insert(
            *pw::keys::NODE_NICK,
            "Internal Mono Speaker + Haptic Feedback",
        );
        let parent = pw::stream::StreamBox::new(&core, "punktfunk-pad-audio", parent_props)
            .context("pw pad-sink parent Stream")?;
        let _parent_listener = parent
            .add_local_listener_with_user_data(PadUd {
                mix: mix.clone(),
                node: PadNode::Parent,
            })
            .state_changed({
                let mainloop = mainloop.clone();
                move |_s, _ud, old, new| {
                    tracing::debug!(?old, ?new, "pipewire pad-sink parent state");
                    if matches!(new, pw::stream::StreamState::Error(_)) {
                        mainloop.quit();
                    }
                }
            })
            .param_changed(move |_stream, _ud, id, param| {
                let Some(param) = param else { return };
                if id != pw::spa::param::ParamType::Format.as_raw() {
                    return;
                }
                let mut info = AudioInfoRaw::default();
                if info.parse(param).is_ok() {
                    tracing::info!(
                        format = ?info.format(),
                        rate = info.rate(),
                        channels = info.channels(),
                        "pad-sink parent format negotiated"
                    );
                }
            })
            .process(pad_process!())
            .register()
            .context("register pad-sink parent listener")?;

        let mut haptic_props = base_props("Audio/Sink", &identity.haptic_name, "4", "FL,FR,RL,RR");
        haptic_props.insert("device.profile.name", "HiFi: SpeakerHaptic: sink");
        haptic_props.insert(
            "device.profile.description",
            "Internal Mono Speaker + Haptic Feedback",
        );
        haptic_props.insert(
            *pw::keys::NODE_NICK,
            "Internal Mono Speaker + Haptic Feedback",
        );
        let haptic = pw::stream::StreamBox::new(&core, "punktfunk-pad-audio", haptic_props)
            .context("pw pad-sink haptic Stream")?;
        let _haptic_listener = haptic
            .add_local_listener_with_user_data(PadUd {
                mix: mix.clone(),
                node: PadNode::Haptic,
            })
            .process(pad_process!())
            .register()
            .context("register pad-sink haptic listener")?;

        let mut speaker_props = base_props("Audio/Sink", &identity.speaker_name, "1", "MONO");
        speaker_props.insert("device.profile.name", "HiFi: Speaker: sink");
        speaker_props.insert("device.profile.description", "Internal Mono Speaker");
        speaker_props.insert(*pw::keys::NODE_NICK, "Internal Mono Speaker");
        let speaker = pw::stream::StreamBox::new(&core, "punktfunk-pad-audio", speaker_props)
            .context("pw pad-sink speaker Stream")?;
        let _speaker_listener = speaker
            .add_local_listener_with_user_data(PadUd {
                mix: mix.clone(),
                node: PadNode::Speaker,
            })
            .process(pad_process!())
            .register()
            .context("register pad-sink speaker listener")?;

        // RT_PROCESS: a sink must join its producers' driver group or
        // `process()` never fires on a busy graph (see mic connect in mod.rs).
        let flags = pw::stream::StreamFlags::AUTOCONNECT
            | pw::stream::StreamFlags::MAP_BUFFERS
            | pw::stream::StreamFlags::RT_PROCESS;

        let parent_fmt = format_pod(PAD_CHANNELS, aux_positions())?;
        let mut parent_params = [Pod::from_bytes(&parent_fmt).context("parent pod from bytes")?];
        parent
            .connect(
                spa::utils::Direction::Input,
                None,
                flags,
                &mut parent_params,
            )
            .context("pw pad-sink parent connect")?;

        let haptic_fmt = format_pod(PAD_CHANNELS, positioned_quad())?;
        let mut haptic_params = [Pod::from_bytes(&haptic_fmt).context("haptic pod from bytes")?];
        haptic
            .connect(
                spa::utils::Direction::Input,
                None,
                flags,
                &mut haptic_params,
            )
            .context("pw pad-sink haptic connect")?;

        let speaker_fmt = format_pod(1, mono_position())?;
        let mut speaker_params = [Pod::from_bytes(&speaker_fmt).context("speaker pod from bytes")?];
        speaker
            .connect(
                spa::utils::Direction::Input,
                None,
                flags,
                &mut speaker_params,
            )
            .context("pw pad-sink speaker connect")?;

        let _ = ready.send(Ok(()));
        mainloop.run();
        tracing::debug!("pipewire pad-sink loop exited (capturer dropped)");
        Ok(())
    })();
    if let Err(e) = &result {
        let _ = ready.send(Err(anyhow!("{e:#}")));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pad_mac_is_reversed_display_form_and_per_pad_unique() {
        // Pairing bytes 1..7 are 74 E7 D6 3A 53 35 LSB-first; display reverses.
        assert_eq!(pad_mac(0, false), "35:53:3A:D6:E7:74");
        // The pad index offsets the LOW octet — the LAST display octet.
        assert_eq!(pad_mac(1, false), "35:53:3A:D6:E7:75");
        assert_ne!(pad_mac(2, false), pad_mac(3, false));
        // The Edge has its own base, the MAC its uhid pad reports.
        assert_eq!(pad_mac(0, true), "35:53:3A:D6:E7:94");
    }

    #[test]
    fn identity_carries_every_match_surface() {
        let id = PadSinkIdentity::new(0, false);
        // GE-Proton's `string_contains_dualsense_name` legs, each checked separately.
        assert!(id.speaker_name.contains("Sony_Interactive_Entertainment"));
        assert!(id.speaker_name.contains("Wireless_Controller"));
        assert!(id.speaker_name.contains("DualSense"));
        assert!(id.speaker_name.starts_with(
            "alsa_output.usb-Sony_Interactive_Entertainment_DualSense_Wireless_Controller-00."
        ));
        // Suffix GE's `is_dualsense_speaker_sink` substring-tests; speaker
        // binding and Windows 4ch format forcing hang off it.
        assert!(id.speaker_name.ends_with("-00.HiFi__Speaker__sink"));
        assert!(id.speaker_name.contains("Speaker__sink"));
        // Positioned sibling must not contain `Speaker__sink` or GE binds the
        // mono controller-effect stream to the quad.
        assert!(!id.haptic_name.contains("Speaker__sink"));
        assert!(id.haptic_name.ends_with("-00.HiFi__SpeakerHaptic__sink"));
        // No colons in a udev serial, and no invented MAC (no USB iSerialNumber;
        // the trailing card index disambiguates).
        assert!(!id.speaker_name.contains(':'));
        assert!(!id.speaker_name.contains("35533AD6E774"));
        // Case-sensitive `wcsstr(FriendlyName, L"Wireless Controller")`.
        assert_eq!(id.description, "Wireless Controller");
        // `0x` prefix: parseable under base 16 and base 0; bare `"0ce6"` is not.
        assert_eq!(id.product_id, "0x0ce6");
        assert_eq!(id.components, "USB054c:0ce6");
        assert_eq!(id.card_name, "DualSense Wireless Controller");
        assert!(id.long_card_name.contains("Sony Interactive Entertainment"));

        let edge = PadSinkIdentity::new(1, true);
        // GE tests Edge with the full `DualSense_Edge_Wireless_Controller` substring.
        assert!(edge
            .speaker_name
            .contains("DualSense_Edge_Wireless_Controller"));
        assert!(edge.speaker_name.contains("Speaker__sink"));
        assert_eq!(edge.product_id, "0x0df2");
        assert_eq!(edge.components, "USB054c:0df2");
        assert_ne!(id.speaker_name, PadSinkIdentity::new(1, false).speaker_name);
        assert!(PadSinkIdentity::new(1, false)
            .speaker_name
            .ends_with("-01.HiFi__Speaker__sink"));
        assert_ne!(id.parent_name, PadSinkIdentity::new(1, false).parent_name);
    }

    #[test]
    fn split_target_points_at_the_parent_unless_overridden() {
        // Public sinks name the hidden parent; the parent names itself.
        let id = PadSinkIdentity::new(0, false);
        assert_eq!(id.split_name, id.parent_name);
        assert_ne!(id.split_name, id.speaker_name);
        // Override via the pure form so tests do not mutate the process env.
        assert_eq!(resolve_split_target("n", None), "n");
        assert_eq!(resolve_split_target("n", Some(" ".into())), "n");
        assert!(resolve_split_target("n", Some("0".into())).is_empty());
        assert!(resolve_split_target("n", Some("off".into())).is_empty());
        assert_eq!(resolve_split_target("n", Some(" other ".into())), "other");
    }

    #[test]
    fn template_expansion() {
        assert_eq!(expand("pad{pad}-{mac}", 2, "AABB"), "pad2-AABB");
        assert_eq!(expand("static", 0, "x"), "static");
    }

    /// Mono speaker is hardware channel 1, not 0 — why the mono sink exists.
    #[test]
    fn channel_maps_follow_the_ucm() {
        assert_eq!(PadNode::Parent.channel_map(), &[0, 1, 2, 3]);
        assert_eq!(PadNode::Haptic.channel_map(), &[0, 1, 2, 3]);
        assert_eq!(PadNode::Speaker.channel_map(), &[1]);
        assert_eq!(PadNode::Speaker.src_channels(), 1);
        assert_eq!(PadNode::Haptic.src_channels(), 4);
    }

    /// One node alone: a round closes when that node contributes again, so the
    /// quad comes out index-exact.
    #[test]
    fn mixer_passes_a_lone_quad_through_unchanged() {
        let (tx, rx) = sync_channel::<Vec<f32>>(8);
        let mut m = Mixer::new(tx);
        m.add(PadNode::Parent, &[0.1, 0.2, 0.3, 0.4]);
        // Nothing emitted yet — the window is open for peers to sum into.
        assert!(rx.try_recv().is_err());
        m.add(PadNode::Parent, &[0.5, 0.6, 0.7, 0.8]);
        assert_eq!(rx.try_recv().unwrap(), vec![0.1, 0.2, 0.3, 0.4]);
    }

    /// GE drives haptics and controller-effect at once; both must survive in one quad.
    #[test]
    fn mixer_sums_concurrent_nodes_into_one_quad() {
        let (tx, rx) = sync_channel::<Vec<f32>>(8);
        let mut m = Mixer::new(tx);
        // Haptics on the positioned sink (coils only).
        m.add(PadNode::Haptic, &[0.0, 0.0, 0.5, 0.25]);
        // Controller-effect on the mono sink, same round.
        m.add(PadNode::Speaker, &[0.75]);
        m.flush();
        // ch1 carries the mono speaker (UCM `Channel0 1`), ch2/3 the coils, ch0 untouched.
        assert_eq!(rx.try_recv().unwrap(), vec![0.0, 0.75, 0.5, 0.25]);
    }

    /// Ragged buffer lengths must not truncate the longer contributor or read out of bounds.
    #[test]
    fn mixer_handles_unequal_frame_counts() {
        let (tx, rx) = sync_channel::<Vec<f32>>(8);
        let mut m = Mixer::new(tx);
        m.add(PadNode::Parent, &[1.0, 0.0, 0.0, 0.0, 2.0, 0.0, 0.0, 0.0]);
        m.add(PadNode::Speaker, &[0.5]);
        m.flush();
        let out = rx.try_recv().unwrap();
        assert_eq!(out.len(), 8);
        assert_eq!(out[0], 1.0);
        assert_eq!(out[1], 0.5); // mono summed into frame 0's ch1
        assert_eq!(out[4], 2.0); // frame 1 survived
        assert_eq!(out[5], 0.0);
    }

    /// A flush must leave no residue behind for the next round to inherit.
    #[test]
    fn mixer_clears_between_rounds() {
        let (tx, rx) = sync_channel::<Vec<f32>>(8);
        let mut m = Mixer::new(tx);
        m.add(PadNode::Parent, &[1.0, 1.0, 1.0, 1.0]);
        m.flush();
        assert_eq!(rx.try_recv().unwrap(), vec![1.0, 1.0, 1.0, 1.0]);
        m.add(PadNode::Parent, &[0.25, 0.0, 0.0, 0.0]);
        m.flush();
        assert_eq!(rx.try_recv().unwrap(), vec![0.25, 0.0, 0.0, 0.0]);
        // An empty flush emits nothing at all (a quiet pad must not manufacture chunks).
        m.flush();
        assert!(rx.try_recv().is_err());
    }
}
