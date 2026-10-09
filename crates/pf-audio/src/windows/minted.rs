//! Minted punktfunk-owned audio endpoints — own instances of Valve's streaming-audio
//! drivers, not Steam's primaries and not a bundled VB-Cable.
//!
//! * Speakers (`SteamStreamingSpeakers.inf`): silent host sink; WASAPI loopback feeds the encoder.
//! * Microphone (`SteamStreamingMicrophone.inf`): host writes decoded voice to render; capture
//!   is the mic host apps record.
//!
//! A background worker mints one `ROOT\MEDIA` devnode per role and process audio identity.
//! `PunktfunkAudioRole` owns the node; seat hosts also match `PunktfunkAudioSeat` from a
//! validated `PUNKTFUNK_SEAT_ID`. [`minted_ids`] publishes the endpoint ids for the wiring
//! plan. Missing Steam drivers, a denied install, or `PUNKTFUNK_NO_AUDIO_MINT` leaves the ids
//! empty and keeps the name-based ladder.
//!
//! Endpoints persist across host restarts and re-resolve by marker. Evidence:
//! `design/windows-audio-endpoints-and-vbcable.md`. Probe: `punktfunk-host audio-probe mint`.

use super::devnode_api as da;
use super::pad_endpoint as pe;
use super::{audio_control, wiring_plan};
use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

/// Durable ownership marker. The uninstall sweep matches every minted role on it.
pub const ROLE_MARKER: &str = "PunktfunkAudioRole";
/// Secondary marker that partitions minted roles between validated seat hosts.
pub const SEAT_MARKER: &str = "PunktfunkAudioSeat";
const SEAT_MARKER_DOMAIN: &[u8] = b"punktfunk/audio-seat/v1\0";
/// Audiosrv can take this long to register a freshly minted endpoint.
const ENDPOINT_WAIT: Duration = Duration::from_secs(15);
/// Floor between retries. [`ensure_provisioned`] is called from wiring passes, which recur freely.
const RETRY_COOLDOWN: Duration = Duration::from_secs(60);
/// Unlatched PnP (re)binds broadcast a device-change every running app services.
/// A box that cannot mint must not pay that on every retry; a service restart re-arms.
const MAX_UNLATCHED_ATTEMPTS: u32 = 5;
/// Wait for an in-flight pass. A cold mint worst-cases around two [`ENDPOINT_WAIT`]s plus stamp settles.
const BLOCKING_WAIT: Duration = Duration::from_secs(90);

/// Persisted marker (`value`) and the INF/hwid needles for [`discover_driver`].
#[derive(Clone, Copy, PartialEq)]
enum Role {
    Speakers,
    Mic,
}

impl Role {
    fn value(self) -> u32 {
        match self {
            Role::Speakers => 1,
            Role::Mic => 2,
        }
    }
    fn desc(self) -> &'static str {
        match self {
            Role::Speakers => "Punktfunk Speakers",
            Role::Mic => "Punktfunk Microphone",
        }
    }
    fn needle(self) -> &'static str {
        match self {
            Role::Speakers => "steamstreamingspeakers",
            Role::Mic => "steamstreamingmicrophone",
        }
    }
    fn inf_name(self) -> &'static str {
        match self {
            Role::Speakers => "SteamStreamingSpeakers.inf",
            Role::Mic => "SteamStreamingMicrophone.inf",
        }
    }
    fn label(self) -> &'static str {
        match self {
            Role::Speakers => "speakers",
            Role::Mic => "mic",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SeatIdentity {
    id: String,
    marker: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AudioIdentity {
    seat: Option<SeatIdentity>,
    role_descs: [String; 2],
}

impl AudioIdentity {
    fn from_seat_id(raw: Option<&str>) -> std::result::Result<Self, &'static str> {
        let seat = raw
            .map(pf_paths::seat::validate_seat_id)
            .transpose()?
            .map(|id| SeatIdentity {
                marker: derive_seat_marker(id),
                id: id.to_owned(),
            });
        let role_descs = match seat.as_ref() {
            Some(seat) => [
                format!("Punktfunk Speakers [seat {}]", seat.id),
                format!("Punktfunk Microphone [seat {}]", seat.id),
            ],
            None => [
                Role::Speakers.desc().to_string(),
                Role::Mic.desc().to_string(),
            ],
        };
        Ok(Self { seat, role_descs })
    }

    fn label(&self) -> &str {
        self.seat.as_ref().map_or("console", |seat| &seat.id)
    }

    fn seat_marker(&self) -> Option<u32> {
        self.seat.as_ref().map(|seat| seat.marker)
    }

    fn role_desc(&self, role: Role) -> &str {
        match role {
            Role::Speakers => &self.role_descs[0],
            Role::Mic => &self.role_descs[1],
        }
    }

    fn thread_name(&self) -> String {
        match self.seat.as_ref() {
            Some(seat) => format!("punktfunk-audio-mint-{}", seat.id),
            None => "punktfunk-audio-mint".into(),
        }
    }
}

/// SHA-256's first DWORD over the canonical id and fixed domain is the durable on-disk format.
fn derive_seat_marker(validated_id: &str) -> u32 {
    let mut hash = Sha256::new();
    hash.update(SEAT_MARKER_DOMAIN);
    hash.update(validated_id.as_bytes());
    let digest = hash.finalize();
    u32::from_be_bytes([digest[0], digest[1], digest[2], digest[3]])
}

/// Console nodes carry only the role; seat nodes must carry the exact marker pair.
fn markers_match(
    identity: &AudioIdentity,
    role: Role,
    found_role: Option<u32>,
    found_seat: Option<u32>,
) -> bool {
    found_role == Some(role.value()) && found_seat == identity.seat_marker()
}

fn process_identity() -> Result<&'static AudioIdentity> {
    static IDENTITY: OnceLock<std::result::Result<AudioIdentity, &'static str>> = OnceLock::new();
    let parsed = IDENTITY.get_or_init(|| {
        pf_paths::seat::seat_id().and_then(|id| AudioIdentity::from_seat_id(id.as_deref()))
    });
    match parsed {
        Ok(identity) => Ok(identity),
        Err(message) => bail!("{message}"),
    }
}

/// Partial is usable: one driver leg failing must not cost the other role.
#[derive(Debug, Default, Clone)]
pub struct MintedAudio {
    pub speakers_devnode: Option<String>,
    pub speakers_render: Option<String>,
    pub mic_devnode: Option<String>,
    pub mic_render: Option<String>,
    pub mic_capture: Option<String>,
}

impl MintedAudio {
    fn any(&self) -> bool {
        self.speakers_render.is_some() || self.mic_render.is_some()
    }
}

/// Latch only a non-empty result. An empty latch freezes a transient failure for the process lifetime.
static PROVISIONED: OnceLock<Arc<MintedAudio>> = OnceLock::new();
/// Keeps concurrent askers to one worker.
static PROVISIONING: AtomicBool = AtomicBool::new(false);
/// When the last attempt started — the [`RETRY_COOLDOWN`] anchor, not when it finished.
static LAST_ATTEMPT: Mutex<Option<Instant>> = Mutex::new(None);
/// Give-up counter for [`MAX_UNLATCHED_ATTEMPTS`], shared by the worker and the blocking path.
static UNLATCHED_ATTEMPTS: AtomicU32 = AtomicU32::new(0);

/// Count one finished-but-unlatched pass. The crossing attempt logs the give-up exactly once.
fn record_unlatched_attempt() {
    let n = UNLATCHED_ATTEMPTS.fetch_add(1, Ordering::SeqCst) + 1;
    if n == MAX_UNLATCHED_ATTEMPTS {
        tracing::warn!(
            attempts = n,
            "minted-audio provisioning keeps failing — giving up for this host lifetime so \
             retries stop broadcasting device changes at the whole box; the wiring plan keeps \
             the name-based ladder, a service restart re-arms minting"
        );
    }
}

fn gave_up() -> bool {
    UNLATCHED_ATTEMPTS.load(Ordering::SeqCst) >= MAX_UNLATCHED_ATTEMPTS
}

/// `PUNKTFUNK_NO_AUDIO_MINT=1`: keep the name-based ladder; `=0` mints as if unset.
fn mint_opted_out() -> bool {
    pf_host_config::env_on("PUNKTFUNK_NO_AUDIO_MINT") == Some(true)
}

/// Wiring-plan tier-0: minted endpoint ids, or all-empty while nothing is provisioned.
pub fn minted_ids() -> wiring_plan::MintedIds {
    match PROVISIONED.get() {
        Some(m) => wiring_plan::MintedIds {
            speakers_render: m.speakers_render.clone(),
            mic_render: m.mic_render.clone(),
            mic_capture: m.mic_capture.clone(),
        },
        None => wiring_plan::MintedIds::default(),
    }
}

/// The full record including devnode instance ids. [`minted_ids`] is the wiring-plan subset.
pub fn provisioned() -> Option<Arc<MintedAudio>> {
    PROVISIONED.get().cloned()
}

/// Starts one process-identity provisioning worker. Idempotent and non-blocking.
pub fn provision_at_startup() {
    if mint_opted_out() || gave_up() {
        return;
    }
    if PROVISIONED.get().is_some() || PROVISIONING.swap(true, Ordering::SeqCst) {
        return;
    }
    *LAST_ATTEMPT.lock().unwrap() = Some(Instant::now());
    let identity = match process_identity() {
        Ok(identity) => identity,
        Err(e) => {
            PROVISIONING.store(false, Ordering::SeqCst);
            tracing::warn!(error = %e,
                "minted-audio provisioning rejected the seat identity — the wiring plan keeps \
                 the name-based ladder");
            record_unlatched_attempt();
            return;
        }
    };
    let spawned = thread::Builder::new()
        .name(identity.thread_name())
        .spawn(move || {
            match ensure_all(identity) {
                Ok(m) if m.any() => {
                    tracing::info!(
                        seat = identity.label(),
                        speakers = m.speakers_render.as_deref().unwrap_or("-"),
                        mic_render = m.mic_render.as_deref().unwrap_or("-"),
                        mic_capture = m.mic_capture.as_deref().unwrap_or("-"),
                        "minted audio endpoints ready (the wiring plan's tier-0)"
                    );
                    let _ = PROVISIONED.set(Arc::new(m));
                }
                Ok(_) => {
                    tracing::info!(
                        seat = identity.label(),
                        "no minted audio endpoints (Steam's streaming drivers absent?) — the \
                         wiring plan keeps the name-based ladder"
                    );
                    record_unlatched_attempt();
                }
                Err(e) => {
                    tracing::warn!(seat = identity.label(), error = %format!("{e:#}"),
                        "minted-audio provisioning failed — the wiring plan keeps the name-based \
                         ladder and a later wiring pass retries");
                    record_unlatched_attempt();
                }
            }
            PROVISIONING.store(false, Ordering::SeqCst);
        });
    if let Err(e) = spawned {
        PROVISIONING.store(false, Ordering::SeqCst);
        tracing::warn!(seat = identity.label(), error = %e,
            "minted-audio provisioning thread not spawned");
    }
}

/// Wiring-pass retry. Cheap once latched; while unlatched, at most every [`RETRY_COOLDOWN`] so a late Steam install still mints.
pub fn ensure_provisioned() {
    if PROVISIONED.get().is_some() || gave_up() {
        return;
    }
    {
        let last = LAST_ATTEMPT.lock().unwrap();
        if last.is_some_and(|t| t.elapsed() < RETRY_COOLDOWN) {
            return;
        }
    }
    provision_at_startup();
}

/// Provisions both roles for one validated identity; one failed role leaves the other usable.
fn ensure_all(identity: &'static AudioIdentity) -> Result<MintedAudio> {
    wasapi::initialize_mta()
        .ok()
        .context("CoInitializeEx (MTA, minted-audio)")?;
    let mut out = MintedAudio::default();
    for role in [Role::Speakers, Role::Mic] {
        match ensure_role(identity, role) {
            Ok((devnode, render, capture)) => match role {
                Role::Speakers => {
                    out.speakers_devnode = Some(devnode);
                    out.speakers_render = Some(render);
                }
                Role::Mic => {
                    out.mic_devnode = Some(devnode);
                    out.mic_render = Some(render);
                    out.mic_capture = capture;
                }
            },
            Err(e) => tracing::info!(seat = identity.label(), role = role.label(),
                error = %format!("{e:#}"), "minted-audio role unavailable"),
        }
    }
    Ok(out)
}

/// Put either minted mic pin back on the stamped 16-bit stereo 48 kHz device format. Steam's
/// driver hands render bytes to the capture pin raw, so a pin at another depth or width turns
/// the mic into noise. A property stamp is only served after an audio-service restart;
/// `SetDeviceFormat` is served at once. Runs before the virtual mic opens its stream.
pub fn repair_mic_formats() {
    let Some(m) = provisioned() else {
        return;
    };
    for id in [m.mic_render.as_deref(), m.mic_capture.as_deref()]
        .into_iter()
        .flatten()
    {
        let served = pe::served_blob(id, &pe::PKEY_DEVICE_FORMAT);
        if served
            .as_deref()
            .is_some_and(|f| same_pcm_shape(f, &WFX_PCM16_2CH_48K))
        {
            continue;
        }
        let pcm16 = [(16, 16, wasapi::SampleType::Int)];
        match audio_control::set_endpoint_format(id, 2, 48_000, &[0x3], &pcm16) {
            Ok(()) => {
                tracing::warn!(endpoint = id,
                "virtual mic pin was not on 16-bit stereo 48 kHz — put it back (a mismatched pin \
                 turns the mic into noise)")
            }
            Err(e) => tracing::warn!(endpoint = id, error = %format!("{e:#}"),
                "virtual mic pin is off its format and couldn't be put back — the mic may carry \
                 noise until the pin is set to 16-bit stereo 48 kHz"),
        }
    }
}

/// Same channels, rate, sample depth and subtype (the fields the driver's raw copy depends
/// on). Byte equality would also compare fields the audio service rewrites on its own.
fn same_pcm_shape(served: &[u8], want: &[u8; 40]) -> bool {
    let field = |b: &[u8], r: std::ops::Range<usize>| b.get(r).map(<[u8]>::to_vec);
    [2..4, 4..8, 14..16, 24..40]
        .into_iter()
        .all(|r| field(served, r.clone()).is_some() && field(served, r.clone()) == field(want, r))
}

/// Reuses this identity's healthy marker pair or creates it, then restores changed defaults.
fn ensure_role(
    identity: &'static AudioIdentity,
    role: Role,
) -> Result<(String, String, Option<String>)> {
    // A healthy endpoint skips the PnP rebind, whose broadcast makes games rebuild audio graphs.
    if let Some((devnode, render, capture)) = find_healthy_role(identity, role)? {
        stamp_identity(&render, identity, role, false);
        if let Some(cap) = capture.as_ref() {
            stamp_identity(cap, identity, role, true);
        }
        return Ok((devnode, render, capture));
    }

    let prev_render = audio_control::default_render_id();
    let prev_capture = audio_control::default_capture_id();

    let (hwid, inf) = discover_driver(role.needle(), role.inf_name())?;
    let devnode = match find_role_devnode(identity, role)? {
        Some(inst) => inst,
        None => {
            // An unmarked ROOT\MEDIA node does not prove which process made it. Only the legacy
            // console path recovers one; a seat always mints its own marker pair.
            let orphan = if identity.seat.is_none() {
                adopt_console_orphan_devnode(role, &hwid)?
            } else {
                None
            };
            match orphan {
                Some(inst) => inst,
                None => {
                    let inst =
                        da::create_media_devnode(identity.role_desc(role), &hwid, |set, did| {
                            // The role remains the ownership marker even if the seat write fails —
                            // and the node is already registered, so failing here would orphan
                            // one nothing can find again per retry.
                            da::write_devparam_dword(set, did, ROLE_MARKER, role.value())?;
                            if let Some(marker) = identity.seat_marker()
                                && let Err(e) =
                                    da::write_devparam_dword(set, did, SEAT_MARKER, marker)
                            {
                                tracing::warn!(error = %format!("{e:#}"),
                                    "audio devnode: seat marker not written — the node binds \
                                     now; a later host start re-mints it");
                            }
                            Ok(())
                        })?;
                    tracing::info!(seat = identity.label(), role = role.label(), devnode = %inst,
                        "minted an audio devnode");
                    inst
                }
            }
        }
    };
    // Once bound, an endpoint can take a default before a later step fails: the restore below
    // runs on every exit from here.
    let endpoints = (|| -> Result<(String, Option<String>)> {
        da::bind_driver(&hwid, &inf)?;
        let render = pe::wait_for_endpoint(&devnode, wasapi::Direction::Render, ENDPOINT_WAIT)?;
        let capture = match role {
            Role::Mic => Some(
                pe::wait_for_endpoint(&devnode, wasapi::Direction::Capture, ENDPOINT_WAIT)
                    .with_context(|| {
                        format!("the minted mic devnode {devnode} produced no capture endpoint")
                    })?,
            ),
            Role::Speakers => None,
        };
        stamp_identity(&render, identity, role, false);
        if let Some(cap) = capture.as_ref() {
            stamp_identity(cap, identity, role, true);
        }
        Ok((render, capture))
    })();

    // A fresh endpoint can grab a default; routing policy belongs to the wiring plan. Undo only
    // that grab: a capture that parked the default meanwhile made its own choice.
    let grabbed = |now: Option<String>, prev: &str| {
        now.is_some_and(|now| {
            now != prev
                && match &endpoints {
                    Ok((render, capture)) => {
                        now == *render || capture.as_deref() == Some(now.as_str())
                    }
                    Err(_) => !super::wasapi_cap::capture_live(),
                }
        })
    };
    if let Some(prev) = prev_render {
        if grabbed(audio_control::default_render_id(), &prev)
            && audio_control::set_default_endpoint(&prev).is_ok()
        {
            tracing::info!(
                seat = identity.label(),
                role = role.label(),
                "default playback restored after minting"
            );
        }
    }
    if let Some(prev) = prev_capture {
        if grabbed(audio_control::default_capture_id(), &prev)
            && audio_control::set_default_endpoint(&prev).is_ok()
        {
            tracing::info!(
                seat = identity.label(),
                role = role.label(),
                "default recording restored after minting"
            );
        }
    }
    let (render, capture) = endpoints?;
    Ok((devnode, render, capture))
}

/// Returns this identity's marker-matched devnode only when all role endpoints are registered.
fn find_healthy_role(
    identity: &AudioIdentity,
    role: Role,
) -> Result<Option<(String, String, Option<String>)>> {
    let Some(devnode) = find_role_devnode(identity, role)? else {
        return Ok(None);
    };
    let Some(render) = pe::find_endpoint_for_devnode(&devnode)? else {
        return Ok(None);
    };
    let capture = match role {
        Role::Mic => match pe::find_capture_endpoint_for_devnode(&devnode)? {
            Some(cap) => Some(cap),
            None => return Ok(None),
        },
        Role::Speakers => None,
    };
    Ok(Some((devnode, render, capture)))
}

/// Stamp/settle passes before accepting "stored but not yet served". A settled endpoint takes the first; a fresh one may wait for Audiosrv.
const STAMP_ATTEMPTS: usize = 3;
/// Gap between a stamp write and the served-check. Immediate reads report success on writes the stack later reverts.
const STAMP_SETTLE: Duration = Duration::from_millis(1200);

/// Stereo 48 kHz f32 `WAVEFORMATEXTENSIBLE` — both sides of the minted mic declare this mix format.
///
/// The driver forwards the render stream raw into capture. A mono-stamped render is unopenable
/// (`AUDCLNT_E_UNSUPPORTED_FORMAT`); a stereo render against a mono capture default plays an
/// octave low. Pin both sides coherent.
const WFX_F32_2CH_48K: [u8; 40] = [
    0xfe, 0xff, // wFormatTag = WAVE_FORMAT_EXTENSIBLE
    0x02, 0x00, // nChannels = 2
    0x80, 0xbb, 0x00, 0x00, // nSamplesPerSec = 48000
    0x00, 0xdc, 0x05, 0x00, // nAvgBytesPerSec = 384000
    0x08, 0x00, // nBlockAlign = 8
    0x20, 0x00, // wBitsPerSample = 32
    0x16, 0x00, // cbSize = 22
    0x20, 0x00, // wValidBitsPerSample = 32
    0x03, 0x00, 0x00, 0x00, // dwChannelMask = FL | FR
    0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b,
    0x71, // KSDATAFORMAT_SUBTYPE_IEEE_FLOAT
];
/// Device-format leg: 16-bit PCM stereo. A float device-format is one of the incoherent sets that make the endpoint unopenable.
const WFX_PCM16_2CH_48K: [u8; 40] = [
    0xfe, 0xff, // wFormatTag = WAVE_FORMAT_EXTENSIBLE
    0x02, 0x00, // nChannels = 2
    0x80, 0xbb, 0x00, 0x00, // nSamplesPerSec = 48000
    0x00, 0xee, 0x02, 0x00, // nAvgBytesPerSec = 192000
    0x04, 0x00, // nBlockAlign = 4
    0x10, 0x00, // wBitsPerSample = 16
    0x16, 0x00, // cbSize = 22
    0x10, 0x00, // wValidBitsPerSample = 16
    0x03, 0x00, 0x00, 0x00, // dwChannelMask = FL | FR
    0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b,
    0x71, // KSDATAFORMAT_SUBTYPE_PCM
];

/// Best-effort identity and mic-format stamps; wiring still keys off the endpoint id.
/// A wider property set makes AudioEndpointBuilder replace the endpoint GUID.
fn stamp_identity(endpoint_id: &str, identity: &'static AudioIdentity, role: Role, capture: bool) {
    let mut stamps = vec![
        pe::Stamp {
            label: "device-desc",
            key: pe::PKEY_DEVICE_DESC,
            value: pe::StampValue::Str(identity.role_desc(role)),
        },
        pe::Stamp {
            label: "device-name",
            key: pe::PKEY_ENDPOINT_DEVICE_NAME,
            value: pe::StampValue::Str("Punktfunk"),
        },
    ];
    // Both mic pins are stereo 48 kHz, and every mix-format copy a pin carries must agree with
    // its device format. Left on the driver's 44.1 kHz mono, the capture pin refuses its own
    // mix format and Control Panel's Recording tab hangs on it.
    if role == Role::Mic {
        stamps.extend([
            pe::Stamp {
                label: "device-format",
                key: pe::PKEY_DEVICE_FORMAT,
                value: pe::StampValue::Format(&WFX_PCM16_2CH_48K),
            },
            pe::Stamp {
                label: "host-format",
                key: pe::PKEY_HOST_FORMAT,
                value: pe::StampValue::Format(&WFX_F32_2CH_48K),
            },
            pe::Stamp {
                label: "mix-format-3",
                key: pe::PKEY_MIX_FORMAT_3,
                value: pe::StampValue::Format(&WFX_F32_2CH_48K),
            },
        ]);
        stamps.push(if capture {
            pe::Stamp {
                label: "capture-mix-format",
                key: pe::PKEY_CAPTURE_MIX_FORMAT,
                value: pe::StampValue::Format(&WFX_F32_2CH_48K),
            }
        } else {
            pe::Stamp {
                label: "mix-format-2",
                key: pe::PKEY_MIX_FORMAT_2,
                value: pe::StampValue::Format(&WFX_F32_2CH_48K),
            }
        });
    }
    // Served stamps need no writes or settle delay on later boots.
    if pe::stamps_served(endpoint_id, &stamps) {
        return;
    }
    for attempt in 0..STAMP_ATTEMPTS {
        if let Err(e) = pe::write_stamps(endpoint_id, &stamps) {
            tracing::info!(seat = identity.label(), role = role.label(), endpoint = %endpoint_id,
                error = %format!("{e:#}"),
                "could not stamp the minted endpoint's name (needs the SYSTEM ACL route) — \
                 the endpoint still wires correctly, it just keeps the driver's default name");
            return;
        }
        thread::sleep(STAMP_SETTLE);
        if pe::stamps_served(endpoint_id, &stamps) {
            if attempt > 0 {
                tracing::debug!(
                    seat = identity.label(),
                    role = role.label(),
                    attempt = attempt + 1,
                    "minted endpoint name held after a re-pass"
                );
            }
            return;
        }
    }
    tracing::info!(seat = identity.label(), role = role.label(), endpoint = %endpoint_id,
        "minted endpoint name is stored but not yet served — it appears after the next \
         audio-stack restart or reboot");
}

/// Finds the devnode carrying this identity's exact role and optional seat marker pair.
fn find_role_devnode(identity: &AudioIdentity, role: Role) -> Result<Option<String>> {
    let set = da::media_class_devs()?;
    for did in set.iter() {
        if markers_match(
            identity,
            role,
            da::read_devparam_dword(&set, &did, ROLE_MARKER),
            da::read_devparam_dword(&set, &did, SEAT_MARKER),
        ) {
            if let Some(inst) = da::instance_id(&set, &did) {
                return Ok(Some(inst));
            }
        }
    }
    Ok(None)
}

/// Recovers a console `ROOT\MEDIA\NNNN` with this hwid and no ownership marker.
/// Steam's own nodes use another instance prefix; any marked Punktfunk family stays untouched.
fn adopt_console_orphan_devnode(role: Role, hwid: &str) -> Result<Option<String>> {
    use windows::Win32::Devices::DeviceAndDriverInstallation::SPDRP_HARDWAREID;
    let set = da::media_class_devs()?;
    for mut did in set.iter() {
        let Some(inst) = da::instance_id(&set, &did) else {
            continue;
        };
        if !inst.to_ascii_uppercase().starts_with("ROOT\\MEDIA\\") {
            continue;
        }
        if !da::devnode_multi_sz_prop(&set, &did, SPDRP_HARDWAREID)
            .iter()
            .any(|h| h.eq_ignore_ascii_case(hwid))
        {
            continue;
        }
        if super::devnode_cleanup::OWNER_MARKERS
            .iter()
            .any(|m| da::read_devparam_dword(&set, &did, m).is_some())
        {
            continue;
        }
        da::write_devparam_dword(&set, &mut did, ROLE_MARKER, role.value())?;
        tracing::warn!(
            seat = "console",
            role = role.label(),
            devnode = %inst,
            "adopted an abandoned console audio devnode whose owner marker never landed; \
             re-marked and reused it instead of minting a duplicate"
        );
        return Ok(Some(inst));
    }
    Ok(None)
}

/// Hardware id + INF for one Steam streaming driver: prefer an installed `oemNN.inf` Windows already trusts, else Steam's driver directory.
pub fn discover_driver(needle: &str, inf_name: &str) -> Result<(String, String)> {
    use windows::Win32::Devices::DeviceAndDriverInstallation::SPDRP_HARDWAREID;
    let steam_dir_inf = || -> Option<String> {
        let w = super::wasapi_mic::steam_driver_inf_path(inf_name)?;
        let s = String::from_utf16_lossy(&w)
            .trim_end_matches('\0')
            .to_string();
        std::path::Path::new(&s).exists().then_some(s)
    };
    let set = da::media_class_devs()?;
    for did in set.iter() {
        let Some(hwid) = da::devnode_multi_sz_prop(&set, &did, SPDRP_HARDWAREID)
            .into_iter()
            .find(|h| h.to_lowercase().contains(needle))
        else {
            continue;
        };
        if let Some(inf) = da::devnode_inf_path(&set, &did) {
            let windir = std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".into());
            let full = format!(r"{windir}\INF\{inf}");
            if std::path::Path::new(&full).exists() {
                return Ok((hwid, full));
            }
        }
        // Keep the exact hwid even if this devnode's INF is gone; try Steam's directory.
        if let Some(s) = steam_dir_inf() {
            return Ok((hwid, s));
        }
    }
    // The INF stem is the canonical ROOT hardware id when no matching devnode is installed.
    if let Some(s) = steam_dir_inf() {
        return Ok((format!("ROOT\\{}", inf_name.trim_end_matches(".inf")), s));
    }
    bail!(
        "no installed devnode matches {needle:?} and Steam's driver directory has no \
         {inf_name} — install Steam (it never needs to run)"
    )
}

/// Runs the mic pump's first resolve without racing the startup worker.
///
/// A latched result or `PUNKTFUNK_NO_AUDIO_MINT=1` returns immediately. Otherwise an in-flight
/// pass wins, failed passes respect [`RETRY_COOLDOWN`], and the process attempt cap still applies.
pub fn ensure_blocking() {
    if mint_opted_out() || PROVISIONED.get().is_some() || gave_up() {
        return;
    }
    // One SetupAPI/PnP sweep runs at a time inside this process.
    if PROVISIONING.swap(true, Ordering::SeqCst) {
        let deadline = Instant::now() + BLOCKING_WAIT;
        while PROVISIONING.load(Ordering::SeqCst) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(100));
        }
        return;
    }
    // A failed first resolve leaves the cooldown in force for later callers.
    let run = {
        let mut last = LAST_ATTEMPT.lock().unwrap();
        if last.is_some_and(|t| t.elapsed() < RETRY_COOLDOWN) {
            false
        } else {
            *last = Some(Instant::now());
            true
        }
    };
    if run {
        match process_identity().and_then(ensure_all) {
            Ok(m) if m.any() => {
                let _ = PROVISIONED.set(Arc::new(m));
            }
            Ok(_) => record_unlatched_attempt(),
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"),
                    "blocking minted-audio provisioning failed");
                record_unlatched_attempt();
            }
        }
    }
    PROVISIONING.store(false, Ordering::SeqCst);
}

pub fn devtest_mint() -> Result<()> {
    let identity = process_identity()?;
    let m = ensure_all(identity)?;
    println!("audio-mint: seat={}", identity.label());
    println!(
        "audio-mint: speakers devnode={} render={}",
        m.speakers_devnode.as_deref().unwrap_or("-"),
        m.speakers_render.as_deref().unwrap_or("-")
    );
    println!(
        "audio-mint: mic devnode={} render={} capture={}",
        m.mic_devnode.as_deref().unwrap_or("-"),
        m.mic_render.as_deref().unwrap_or("-"),
        m.mic_capture.as_deref().unwrap_or("-")
    );
    if m.any() {
        let _ = PROVISIONED.set(Arc::new(m));
        println!(
            "audio-mint: published for this process — `audio-probe plan` shows the tier-0 pick"
        );
    } else {
        println!("audio-mint: nothing minted (Steam's streaming drivers absent?)");
    }
    Ok(())
}

#[cfg(test)]
mod seat_tests {
    use super::*;

    fn identity(id: Option<&str>) -> AudioIdentity {
        AudioIdentity::from_seat_id(id).unwrap()
    }

    #[test]
    fn seat_marker_derivation_is_stable() {
        let first = identity(Some("550e8400e29b41d4a716446655440000"));
        let repeated = identity(Some("550e8400e29b41d4a716446655440000"));
        let other = identity(Some("550e8400e29b41d4a716446655440001"));

        assert_eq!(first, repeated);
        assert_eq!(first.seat_marker(), Some(0x0676_e391));
        assert_ne!(first.seat_marker(), other.seat_marker());
        assert_eq!(
            first.role_desc(Role::Speakers),
            "Punktfunk Speakers [seat 550e8400e29b41d4a716446655440000]"
        );
    }

    #[test]
    fn seat_ids_reject_unsafe_display_and_log_text() {
        for raw in [
            "",
            "550E8400E29B41D4A716446655440000",
            "550e8400-e29b-41d4-a716-446655440000",
            "seat 1",
            "seat/1",
            "seat.1",
            "seat\n1",
            "séat-1",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        ] {
            assert!(AudioIdentity::from_seat_id(Some(raw)).is_err(), "{raw:?}");
        }
        assert!(AudioIdentity::from_seat_id(Some("0123456789abcdef0123456789abcdef")).is_ok());
    }

    #[test]
    fn marker_matching_keeps_console_and_seats_separate() {
        let console = identity(None);
        let seat_a = identity(Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"));
        let seat_b = identity(Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"));
        let marker_a = seat_a.seat_marker();
        let marker_b = seat_b.seat_marker();

        assert!(markers_match(&console, Role::Speakers, Some(1), None));
        assert!(!markers_match(&console, Role::Speakers, Some(1), marker_a));
        assert!(markers_match(&seat_a, Role::Speakers, Some(1), marker_a));
        assert!(!markers_match(&seat_a, Role::Speakers, Some(1), None));
        assert!(!markers_match(&seat_a, Role::Speakers, Some(1), marker_b));
        assert!(!markers_match(&seat_a, Role::Speakers, Some(2), marker_a));
        assert!(!markers_match(&seat_a, Role::Speakers, None, None));
        assert!(markers_match(&seat_b, Role::Mic, Some(2), marker_b));
    }
}

#[cfg(test)]
mod format_tests {
    use super::*;

    /// Depth, width and rate each make a different pin; a rewritten byte rate alone does not.
    #[test]
    fn only_the_raw_copy_fields_decide_the_mic_shape() {
        assert!(same_pcm_shape(&WFX_PCM16_2CH_48K, &WFX_PCM16_2CH_48K));
        let mut deep = WFX_PCM16_2CH_48K;
        deep[14] = 24;
        assert!(!same_pcm_shape(&deep, &WFX_PCM16_2CH_48K));
        let mut rate = WFX_PCM16_2CH_48K;
        rate[4..8].copy_from_slice(&44_100u32.to_le_bytes());
        assert!(!same_pcm_shape(&rate, &WFX_PCM16_2CH_48K));
        assert!(!same_pcm_shape(
            &WFX_PCM16_2CH_48K[..18],
            &WFX_PCM16_2CH_48K
        ));
        let mut avg = WFX_PCM16_2CH_48K;
        avg[8] ^= 1;
        assert!(same_pcm_shape(&avg, &WFX_PCM16_2CH_48K));
    }
}
