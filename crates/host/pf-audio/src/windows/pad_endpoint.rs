//! DualSense pad-audio endpoint provisioning (Windows).
//!
//! Mints a per-pad render endpoint games recognise as the pad SPEAKER, then
//! loopback-captures what they play. Games match `PKEY_Device_ContainerId` to
//! the pad HID container and/or a FriendlyName containing "Wireless Controller";
//! format is 4 ch / 48 kHz. The virtual pad stamps PFDS
//! `{50464453-0000-0000-0000-00000000000<idx>}` (`pf-inject` `create_swdevice`).
//!
//! [`ensure`] (host startup, not per session) creates one extra Steam Streaming
//! Speakers devnode per slot (`ROOT\SteamStreamingSpeakers`). Register with
//! `SetupDiRegisterDeviceInfo`, not `DIF_REGISTERDEVICE` — that needs an
//! interactive window station and fails with 1459 from a service. Persist the
//! slot in `Device Parameters\PunktfunkPadIndex`; the INF rewrites DeviceDesc.
//! Stamp only description, name, PFDS container, and the 4 ch/48 kHz triplet.
//! Hardware-id or devicepath writes make AudioEndpointBuilder delete and remint
//! under a new GUID. [`super::pad_capture`] captures. [`set_visibility`] parks
//! `DEVICE_STATE_DISABLED` with no client pad attached; idle libScePad titles
//! stall on a visible DualSense-named speaker. Flips raise no PnP. Wiring-plan
//! exclusion is [`is_pad_render_endpoint`]. COM objects stay on the thread
//! that made them.

use super::audio_control;
use super::devnode_api::{
    bind_driver, create_media_devnode, devnode_inf_path, devnode_multi_sz_prop, instance_id,
    media_class_devs, pv_blob, pv_bytes, pv_clsid, pv_guid, pv_lpwstr, pv_string,
    read_devparam_dword, wide, write_devparam_dword, DevInfoSet,
};
use anyhow::{anyhow, bail, Context, Result};
use std::collections::HashSet;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};
use windows::core::{Owned, GUID, HSTRING, PCWSTR, PWSTR};
use windows::Win32::Devices::DeviceAndDriverInstallation::{SPDRP_HARDWAREID, SP_DEVINFO_DATA};
use windows::Win32::Foundation::PROPERTYKEY;
use windows::Win32::Media::Audio::{
    IAudioClient, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator,
};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL, STGM_READ, STGM_READWRITE};
use windows::Win32::System::Registry::{KEY_QUERY_VALUE, KEY_SET_VALUE};
use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;

/// Data1 of the per-pad container GUID. Must equal pf-inject's `container_tag`
/// or games never match the endpoint to the pad.
pub const PFDS_TAG: u32 = 0x5046_4453;
/// Creation DeviceDesc. The INF overwrites it; [`PAD_INDEX_VALUE`] is the durable marker.
const DEVNODE_DESC: &str = "Punktfunk Pad Audio";
const SSS_HWID: &str = "ROOT\\SteamStreamingSpeakers";
/// Devnode `Device Parameters` slot (REG_DWORD). The uninstall sweep matches on it.
pub const PAD_INDEX_VALUE: &str = "PunktfunkPadIndex";
pub const MMDEV_RENDER_PATH: &str =
    r"SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\Render";
pub const MMDEV_CAPTURE_PATH: &str =
    r"SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\Capture";
const ENDPOINT_ID_PREFIX: &str = "{0.0.0.00000000}.";
/// Capture ids use `{0.0.1.…}`. A render prefix never string-matches, so the
/// minted mic's capture side resolves to nothing.
const CAPTURE_ENDPOINT_ID_PREFIX: &str = "{0.0.1.00000000}.";
/// Audiosrv registers the endpoint asynchronously after driver install.
const ENDPOINT_WAIT: Duration = Duration::from_secs(10);
/// AEB reverts format keys on a fresh endpoint; [`ensure`] re-stamps this many
/// times before asking for an AEB kick.
const STAMP_ATTEMPTS: usize = 5;
/// AEB settle after a stamp. An immediate check reports success even on passes that get reverted.
const STAMP_SETTLE: Duration = Duration::from_millis(1200);

/// One provisioned pad-audio endpoint. Persistent across host restarts;
/// [`remove`] is tests and the `pad-endpoint remove` hatch only.
#[derive(Debug, Clone)]
pub struct PadEndpoint {
    /// WASAPI id. Empty from [`find`] when the devnode exists but the endpoint never registered.
    pub endpoint_id: String,
    pub device_instance: String,
    /// Byte 23 of the stamped container GUID.
    pub pad_index: u8,
    /// Stamp stored but not served: AudioEndpointBuilder + Audiosrv must restart.
    /// Startup may do one restart; nobody else acts on this mid-flight.
    pub needs_aeb_kick: bool,
}

/// One property-store key to stamp. Shared with the minted-audio provider via [`write_stamps`].
pub struct Stamp {
    pub label: &'static str,
    pub key: PROPERTYKEY,
    pub value: StampValue,
}

pub enum StampValue {
    Str(&'static str),
    /// PFDS container (VT_CLSID / serialized-CLSID registry blob).
    Container(GUID),
    /// `WAVEFORMATEXTENSIBLE` (VT_BLOB / serialized-blob registry value).
    Format(&'static [u8; 40]),
}

const fn pkey(fmtid: u128, pid: u32) -> PROPERTYKEY {
    PROPERTYKEY {
        fmtid: GUID::from_u128(fmtid),
        pid,
    }
}

/// Description half of the endpoint display name.
pub const PKEY_DEVICE_DESC: PROPERTYKEY = pkey(0xa45c254e_df1c_4efd_8020_67d146a850e0, 2);
/// Device-name half of the display name.
pub const PKEY_ENDPOINT_DEVICE_NAME: PROPERTYKEY = pkey(0xb3f8fa53_0004_438e_9003_51a46e139bfc, 6);
/// Endpoint-store devnode link: `"{1}.<device instance id>"`.
const PKEY_ENDPOINT_DEVNODE: PROPERTYKEY = pkey(0xb3f8fa53_0004_438e_9003_51a46e139bfc, 2);
/// What games match against the pad's HID container.
const PKEY_CONTAINER_ID: PROPERTYKEY = pkey(0x8c7ed206_3f8a_4827_b3ab_ae9e1faefc6c, 2);
/// 16-bit PCM leg of the format set.
pub const PKEY_DEVICE_FORMAT: PROPERTYKEY = pkey(0xf19f064d_082c_4e27_bc73_6882a1bb8e4c, 0);
/// Float-leg format pair — pids 2 and 3 of the same fmtid.
pub const PKEY_MIX_FORMAT_2: PROPERTYKEY = pkey(0x3d6e1656_2e50_4c4c_8d85_d0acae3c6c68, 2);
pub const PKEY_MIX_FORMAT_3: PROPERTYKEY = pkey(0x3d6e1656_2e50_4c4c_8d85_d0acae3c6c68, 3);
/// Host processing format (float leg).
pub const PKEY_HOST_FORMAT: PROPERTYKEY = pkey(0xe4870e26_3cc5_4cd2_ba46_ca0a9a70ed04, 0);
/// A capture pin's second mix-format copy, beside [`PKEY_MIX_FORMAT_3`]. Render pins lack it.
pub const PKEY_CAPTURE_MIX_FORMAT: PROPERTYKEY = pkey(0x624f56de_fd24_473e_814a_de40aacaed16, 3);

/// 4 ch / 48 kHz / 16-bit PCM, mask 0x33 (FL FR BL BR), PCM subtype.
const WFX_PCM16_4CH_48K: [u8; 40] = [
    0xfe, 0xff, // wFormatTag = WAVE_FORMAT_EXTENSIBLE
    0x04, 0x00, // nChannels = 4
    0x80, 0xbb, 0x00, 0x00, // nSamplesPerSec = 48000
    0x00, 0xdc, 0x05, 0x00, // nAvgBytesPerSec = 384000
    0x08, 0x00, // nBlockAlign = 8
    0x10, 0x00, // wBitsPerSample = 16
    0x16, 0x00, // cbSize = 22
    0x10, 0x00, // wValidBitsPerSample = 16
    0x33, 0x00, 0x00, 0x00, // dwChannelMask = 0x33
    0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b,
    0x71, // KSDATAFORMAT_SUBTYPE_PCM
];
/// 4 ch / 48 kHz / 32-bit float, mask 0x33, IEEE-float subtype.
const WFX_F32_4CH_48K: [u8; 40] = [
    0xfe, 0xff, // wFormatTag = WAVE_FORMAT_EXTENSIBLE
    0x04, 0x00, // nChannels = 4
    0x80, 0xbb, 0x00, 0x00, // nSamplesPerSec = 48000
    0x00, 0xb8, 0x0b, 0x00, // nAvgBytesPerSec = 768000
    0x10, 0x00, // nBlockAlign = 16
    0x20, 0x00, // wBitsPerSample = 32
    0x16, 0x00, // cbSize = 22
    0x20, 0x00, // wValidBitsPerSample = 32
    0x33, 0x00, 0x00, 0x00, // dwChannelMask = 0x33
    0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b,
    0x71, // KSDATAFORMAT_SUBTYPE_IEEE_FLOAT
];

/// Per-pad container GUID — identical to pf-inject's DualSense
/// `GUID::from_values(container_tag, 0, 0, [0,0,0,0,0,0,0,index])`.
pub fn pfds_container_guid(pad_index: u8) -> GUID {
    GUID::from_values(PFDS_TAG, 0, 0, [0, 0, 0, 0, 0, 0, 0, pad_index])
}

/// Minimal stamp set. Hardware-id or devicepath writes make AudioEndpointBuilder
/// delete the endpoint and remint it under a new GUID.
fn stamps_for(pad_index: u8) -> [Stamp; 7] {
    [
        Stamp {
            label: "device-desc",
            key: PKEY_DEVICE_DESC,
            value: StampValue::Str("Wireless Controller"),
        },
        Stamp {
            label: "device-name",
            key: PKEY_ENDPOINT_DEVICE_NAME,
            value: StampValue::Str("DualSense Wireless Controller"),
        },
        Stamp {
            label: "container-id",
            key: PKEY_CONTAINER_ID,
            value: StampValue::Container(pfds_container_guid(pad_index)),
        },
        Stamp {
            label: "device-format",
            key: PKEY_DEVICE_FORMAT,
            value: StampValue::Format(&WFX_PCM16_4CH_48K),
        },
        Stamp {
            label: "mix-format-2",
            key: PKEY_MIX_FORMAT_2,
            value: StampValue::Format(&WFX_F32_4CH_48K),
        },
        Stamp {
            label: "mix-format-3",
            key: PKEY_MIX_FORMAT_3,
            value: StampValue::Format(&WFX_F32_4CH_48K),
        },
        Stamp {
            label: "host-format",
            key: PKEY_HOST_FORMAT,
            value: StampValue::Format(&WFX_F32_4CH_48K),
        },
    ]
}

/// Stamps to apply, honouring `PUNKTFUNK_PAD_AUDIO_STAMPS`.
///
/// Unset means all seven. Set to a comma-separated label list and only those
/// are written. A fully stamped endpoint fails `IAudioClient::Initialize` with
/// `AUDCLNT_E_UNSUPPORTED_FORMAT` for every format including its own mix
/// format; a bare one opens. MMDevices ACL blocks editing in place, so the
/// poison stamp is found by re-provisioning subsets.
fn active_stamps(pad_index: u8) -> Vec<Stamp> {
    let all = stamps_for(pad_index);
    match std::env::var("PUNKTFUNK_PAD_AUDIO_STAMPS") {
        Err(_) => all.into_iter().collect(),
        Ok(list) => {
            let want: HashSet<&str> = list.split(',').map(str::trim).collect();
            all.into_iter().filter(|s| want.contains(s.label)).collect()
        }
    }
}

fn sz_bytes(s: &str) -> Vec<u8> {
    wide(s).iter().flat_map(|u| u.to_le_bytes()).collect()
}

fn guid_str(g: &GUID) -> String {
    format!(
        "{:08x}-{:04x}-{:04x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        g.data1,
        g.data2,
        g.data3,
        g.data4[0],
        g.data4[1],
        g.data4[2],
        g.data4[3],
        g.data4[4],
        g.data4[5],
        g.data4[6],
        g.data4[7]
    )
}

/// MMDevices property-key name: `"{fmtid},pid"`.
fn reg_value_name(k: &PROPERTYKEY) -> String {
    format!("{{{}}},{}", guid_str(&k.fmtid), k.pid)
}

/// Registry byte order (Data1/2/3 LE, Data4 as-is). Byte 23 of the serialized
/// container blob is the pad index.
fn guid_registry_bytes(g: &GUID) -> [u8; 16] {
    let mut out = [0u8; 16];
    out[..4].copy_from_slice(&g.data1.to_le_bytes());
    out[4..6].copy_from_slice(&g.data2.to_le_bytes());
    out[6..8].copy_from_slice(&g.data3.to_le_bytes());
    out[8..].copy_from_slice(&g.data4);
    out
}

/// On-disk shape: strings are REG_SZ; container and formats are serialized
/// PROPVARIANTs (8-byte header `vt, 0x00000001`, then payload) as REG_BINARY.
fn reg_registry_value(v: &StampValue) -> winreg::RegValue<'static> {
    use winreg::enums::{REG_BINARY, REG_SZ};
    match v {
        StampValue::Str(s) => winreg::RegValue {
            bytes: sz_bytes(s).into(),
            vtype: REG_SZ,
        },
        StampValue::Container(g) => {
            let mut b = vec![0x48, 0, 0, 0, 1, 0, 0, 0]; // VT_CLSID header
            b.extend_from_slice(&guid_registry_bytes(g));
            winreg::RegValue {
                bytes: b.into(),
                vtype: REG_BINARY,
            }
        }
        StampValue::Format(wfx) => {
            let mut b = vec![0x41, 0, 0, 0, 1, 0, 0, 0]; // VT_BLOB header
            b.extend_from_slice(&wfx[..]);
            winreg::RegValue {
                bytes: b.into(),
                vtype: REG_BINARY,
            }
        }
    }
}

/// WASAPI id `{0.0.0.00000000}.{guid}` → `{guid}` (MMDevices key name).
/// The uninstall sweep deletes keys by this.
pub fn endpoint_guid_part(endpoint_id: &str) -> Result<&str> {
    endpoint_id
        .rfind('{')
        .map(|i| &endpoint_id[i..])
        .filter(|g| g.len() >= 38 && g.ends_with('}'))
        .ok_or_else(|| anyhow!("unrecognised endpoint id shape: {endpoint_id}"))
}

// `windows 0.62` Drop for PROPVARIANT is PropVariantClear. These variants
// borrow Rust-owned memory (Vec<u16>, &GUID, &'static [u8]); a drop would
// CoTaskMemFree it. ManuallyDrop: never clear borrows. GetValue-owned
// variants clear on drop.

fn devnode_pad_index(set: &DevInfoSet, did: &SP_DEVINFO_DATA) -> Option<u32> {
    read_devparam_dword(set, did, PAD_INDEX_VALUE)
}

/// Find the devnode for `pad_index`. DeviceDesc only survives until the INF installs.
fn find_devnode(pad_index: u8) -> Result<Option<String>> {
    let set = media_class_devs()?;
    for did in set.iter() {
        let Some(inst) = instance_id(&set, &did) else {
            continue;
        };
        if !inst.to_ascii_uppercase().starts_with("ROOT\\") {
            continue;
        }
        if devnode_pad_index(&set, &did) == Some(pad_index as u32) {
            return Ok(Some(inst));
        }
    }
    Ok(None)
}

fn create_devnode(pad_index: u8) -> Result<String> {
    let inst = create_media_devnode(DEVNODE_DESC, SSS_HWID, |set, did| {
        write_pad_index(set, did, pad_index)
    })?;
    tracing::info!(pad = pad_index, devnode = %inst, "created a pad-audio devnode");
    Ok(inst)
}

fn write_pad_index(set: &DevInfoSet, did: &mut SP_DEVINFO_DATA, pad_index: u8) -> Result<()> {
    write_devparam_dword(set, did, PAD_INDEX_VALUE, pad_index as u32)
}

/// Prefer the installed driver's `oemNN.inf` on any SSS-bound devnode; fall
/// back to Steam's driver directory when none exists yet.
fn resolve_sss_inf() -> Result<String> {
    let set = media_class_devs()?;
    for did in set.iter() {
        if !devnode_multi_sz_prop(&set, &did, SPDRP_HARDWAREID)
            .iter()
            .any(|h| h.eq_ignore_ascii_case(SSS_HWID))
        {
            continue;
        }
        if let Some(inf) = devnode_inf_path(&set, &did) {
            let windir = std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".into());
            let full = format!(r"{windir}\INF\{inf}");
            if std::path::Path::new(&full).exists() {
                return Ok(full);
            }
        }
    }
    if let Some(w) = super::wasapi_mic::steam_driver_inf_path("SteamStreamingSpeakers.inf") {
        let s = String::from_utf16_lossy(&w);
        let s = s.trim_end_matches('\0').to_string();
        if std::path::Path::new(&s).exists() {
            return Ok(s);
        }
    }
    bail!(
        "no Steam Streaming Speakers INF found (no installed SSS devnode, and Steam's driver \
         directory is absent) — install Steam, whose Remote Play streaming drivers provide it"
    )
}

fn install_sss_driver() -> Result<()> {
    bind_driver(SSS_HWID, &resolve_sss_inf()?)
}

/// Render endpoint owned by `instance_id`, via the store's `"{1}.<instance id>"` link.
pub fn find_endpoint_for_devnode(instance_id: &str) -> Result<Option<String>> {
    endpoint_for_devnode_in(MMDEV_RENDER_PATH, ENDPOINT_ID_PREFIX, instance_id)
}

/// Capture endpoint owned by `instance_id`. Pad devices are render-only; the
/// minted-audio provider and the `audio-probe` devtest need this direction.
pub fn find_capture_endpoint_for_devnode(instance_id: &str) -> Result<Option<String>> {
    endpoint_for_devnode_in(MMDEV_CAPTURE_PATH, CAPTURE_ENDPOINT_ID_PREFIX, instance_id)
}

fn endpoint_for_devnode_in(
    reg_path: &str,
    id_prefix: &str,
    instance_id: &str,
) -> Result<Option<String>> {
    use winreg::enums::HKEY_LOCAL_MACHINE;
    use winreg::RegKey;
    let want = format!("{{1}}.{instance_id}");
    let root = RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey(reg_path)
        .with_context(|| format!(r"open HKLM\{reg_path}"))?;
    for key in root.enum_keys().flatten() {
        let Ok(props) = root.open_subkey(format!(r"{key}\Properties")) else {
            continue;
        };
        let Ok(link) = props.get_value::<String, _>(reg_value_name(&PKEY_ENDPOINT_DEVNODE)) else {
            continue;
        };
        if link.eq_ignore_ascii_case(&want) {
            return Ok(Some(format!("{id_prefix}{key}")));
        }
    }
    Ok(None)
}

/// Poll until audiosrv has registered `devnode`'s endpoint in `dir`, which it does
/// asynchronously after a driver install.
pub fn wait_for_endpoint(
    devnode: &str,
    dir: wasapi::Direction,
    timeout: Duration,
) -> Result<String> {
    let deadline = Instant::now() + timeout;
    loop {
        let found = match dir {
            wasapi::Direction::Render => find_endpoint_for_devnode(devnode)?,
            wasapi::Direction::Capture => find_capture_endpoint_for_devnode(devnode)?,
        };
        if let Some(ep) = found {
            return Ok(ep);
        }
        if Instant::now() >= deadline {
            let which = match dir {
                wasapi::Direction::Render => "render",
                wasapi::Direction::Capture => "capture",
            };
            bail!(
                "no {which} endpoint appeared for {devnode} within {}s — is Audiosrv running?",
                timeout.as_secs()
            );
        }
        thread::sleep(Duration::from_millis(250));
    }
}

fn open_mmdevice(endpoint_id: &str) -> Result<IMMDevice> {
    let id_w = HSTRING::from(endpoint_id);
    // SAFETY: standard COM activation on a COM-initialized thread; the id buffer is
    // NUL-terminated and outlives the call.
    unsafe {
        let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
            .context("CoCreateInstance(MMDeviceEnumerator)")?;
        en.GetDevice(PCWSTR(id_w.as_ptr()))
            .with_context(|| format!("IMMDeviceEnumerator::GetDevice({endpoint_id})"))
    }
}

/// Wrap [`open_mmdevice`] as a [`wasapi::Device`]. One resolution path so
/// errors name the endpoint id; the raw `IMMDevice` is also what
/// [`probe_activation`] and the property-store readers need.
pub fn open_wasapi_device(endpoint_id: &str) -> Result<wasapi::Device> {
    let dev = open_mmdevice(endpoint_id)?;
    wasapi::Device::from_immdevice(dev)
        .map_err(|e| anyhow!("wrap IMMDevice {endpoint_id} as a wasapi Device: {e}"))
}

/// Log raw `IMMDevice::Activate` vs crate wrap, so a `0x80070002` names
/// the layer (dead endpoint, process cannot activate, bad argument).
pub(super) fn probe_activation(endpoint_id: &str) {
    match open_mmdevice(endpoint_id) {
        Err(e) => tracing::error!(endpoint = %endpoint_id, error = %format!("{e:#}"),
            "activation probe: GetDevice failed"),
        Ok(dev) => {
            // SAFETY: standard COM activation on a COM-initialized thread; the returned
            // interface is dropped immediately (the probe only wants the HRESULT).
            match unsafe { dev.Activate::<IAudioClient>(CLSCTX_ALL, None) } {
                Ok(_) => tracing::info!(endpoint = %endpoint_id,
                    "activation probe: raw IMMDevice::Activate(IAudioClient) OK"),
                Err(e) => tracing::error!(endpoint = %endpoint_id,
                    hr = %format!("{:#010x}", e.code().0),
                    "activation probe: raw IMMDevice::Activate(IAudioClient) FAILED"),
            }
        }
    }
}

fn stamp_served(store: &IPropertyStore, s: &Stamp) -> bool {
    // SAFETY: the key is a valid PROPERTYKEY; GetValue returns an owned variant that clears
    // on drop.
    let Ok(pv) = (unsafe { store.GetValue(&s.key) }) else {
        return false;
    };
    match &s.value {
        StampValue::Str(v) => pv_string(&pv).is_some_and(|got| got == *v),
        StampValue::Container(g) => pv_guid(&pv) == Some(*g),
        StampValue::Format(wfx) => pv_bytes(&pv).is_some_and(|got| got == wfx[..]),
    }
}

fn set_store_value(store: &IPropertyStore, s: &Stamp) -> Result<()> {
    match &s.value {
        StampValue::Str(v) => {
            let buf = wide(v);
            let pv = pv_lpwstr(&buf);
            // SAFETY: the variant borrows `buf`, which outlives the call; SetValue copies.
            unsafe { store.SetValue(&s.key, &*pv) }.context(s.label)?;
        }
        StampValue::Container(g) => {
            let pv = pv_clsid(g);
            // SAFETY: the variant borrows `*g`, which outlives the call; SetValue copies.
            unsafe { store.SetValue(&s.key, &*pv) }.context(s.label)?;
        }
        StampValue::Format(wfx) => {
            let pv = pv_blob(&wfx[..]);
            // SAFETY: the variant borrows the static format bytes; SetValue copies.
            unsafe { store.SetValue(&s.key, &*pv) }.context(s.label)?;
        }
    }
    Ok(())
}

fn stamp_endpoint(endpoint_id: &str, pad_index: u8) -> Result<()> {
    write_stamps(endpoint_id, &active_stamps(pad_index))
}

/// Stamp writer: IPropertyStore first (audiosrv notices immediately), registry
/// for rejects. Already-served keys are skipped. Shared with minted-audio.
pub fn write_stamps(endpoint_id: &str, stamps: &[Stamp]) -> Result<()> {
    let dev = open_mmdevice(endpoint_id)?;
    let pending: Vec<&Stamp> = {
        // SAFETY: read-only property store on a COM-initialized thread.
        let store =
            unsafe { dev.OpenPropertyStore(STGM_READ) }.context("OpenPropertyStore(STGM_READ)")?;
        stamps.iter().filter(|s| !stamp_served(&store, s)).collect()
    };
    if pending.is_empty() {
        tracing::debug!(endpoint = %endpoint_id, "endpoint already fully stamped");
        return Ok(());
    }
    let mut via_store: Vec<&'static str> = Vec::new();
    let mut via_registry: Vec<&Stamp> = Vec::new();
    // SAFETY: read-write property store on a COM-initialized thread (may be denied — handled).
    match unsafe { dev.OpenPropertyStore(STGM_READWRITE) } {
        Ok(rw) => {
            for s in &pending {
                match set_store_value(&rw, s) {
                    Ok(()) => via_store.push(s.label),
                    Err(e) => {
                        tracing::debug!(key = s.label, error = %format!("{e:#}"),
                            "IPropertyStore rejected a pad stamp — registry route");
                        via_registry.push(s);
                    }
                }
            }
            if !via_store.is_empty() {
                // SAFETY: committing the writes above on the same store/thread.
                if let Err(e) = unsafe { rw.Commit() } {
                    // A failed commit may have dropped every store-side write —
                    // re-route all pending through the registry, not half a stamp.
                    tracing::debug!(error = %format!("{e:#}"),
                        "IPropertyStore::Commit failed — registry route for all pending stamps");
                    via_store.clear();
                    via_registry = pending.clone();
                }
            }
        }
        Err(e) => {
            tracing::debug!(error = %format!("{e:#}"),
                "pad endpoint property store not writable — registry route for all stamps");
            via_registry = pending.clone();
        }
    }
    if !via_registry.is_empty() {
        registry_stamp(endpoint_id, &via_registry)?;
    }
    tracing::info!(
        endpoint = %endpoint_id,
        property_store = ?via_store,
        registry = ?via_registry.iter().map(|s| s.label).collect::<Vec<_>>(),
        "endpoint stamped (route per key)"
    );
    Ok(())
}

/// MMDevices `…\Properties` denies writes even to SYSTEM: SYSTEM owns the
/// key but the DACL has no write ACE. Open with READ_CONTROL|WRITE_DAC
/// (owner-implicit), append a FullControl ACE for S-1-5-18, write the DACL
/// back. Resolve principals by SID, never by name — localized Windows
/// fails account-name lookups. Works as SYSTEM; a dev run fails here.
fn grant_system_full_control(subkey_path: &str) -> Result<()> {
    use windows::Win32::Foundation::{HANDLE, HLOCAL};
    use windows::Win32::Security::Authorization::{
        GetSecurityInfo, SetEntriesInAclW, SetSecurityInfo, EXPLICIT_ACCESS_W, GRANT_ACCESS,
        NO_MULTIPLE_TRUSTEE, SE_REGISTRY_KEY, TRUSTEE_IS_SID, TRUSTEE_IS_USER, TRUSTEE_W,
    };
    use windows::Win32::Security::{
        CreateWellKnownSid, WinLocalSystemSid, ACL, CONTAINER_INHERIT_ACE,
        DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SECURITY_MAX_SID_SIZE,
    };
    use windows::Win32::System::Registry::{
        RegOpenKeyExW, HKEY, HKEY_LOCAL_MACHINE, KEY_ALL_ACCESS, REG_SAM_FLAGS,
    };

    const READ_CONTROL: u32 = 0x0002_0000;
    const WRITE_DAC: u32 = 0x0004_0000;
    let path_w = HSTRING::from(subkey_path);
    let mut hkey = HKEY::default();
    // SAFETY: the path is NUL-terminated and outlives the call; hkey is a live out-param.
    unsafe {
        RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(path_w.as_ptr()),
            None,
            REG_SAM_FLAGS(READ_CONTROL | WRITE_DAC),
            &mut hkey,
        )
    }
    .ok()
    .with_context(|| {
        format!("open {subkey_path} for WRITE_DAC (owner-implicit right — requires SYSTEM)")
    })?;
    let handle = HANDLE(hkey.0);
    // SAFETY: the open succeeded, so `hkey` is a key this frame alone owns; `Owned` closes it.
    let _hkey = unsafe { Owned::new(hkey) };
    let mut old_dacl: *mut ACL = std::ptr::null_mut();
    let mut sd = PSECURITY_DESCRIPTOR::default();
    // SAFETY: live handle; out-params are live locals.
    let gs = unsafe {
        GetSecurityInfo(
            handle,
            SE_REGISTRY_KEY,
            DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(&mut old_dacl),
            None,
            Some(&mut sd),
        )
    };
    // SAFETY: `sd` is null or the LocalAlloc'd descriptor just returned, which `old_dacl` points
    // into; `Owned` frees it once, after the last use of either.
    let _sd = unsafe { Owned::new(HLOCAL(sd.0)) };
    gs.ok().context("GetSecurityInfo(DACL)")?;
    let mut sid = [0u8; SECURITY_MAX_SID_SIZE as usize];
    let mut cb = sid.len() as u32;
    // SAFETY: the buffer is SECURITY_MAX_SID_SIZE, the documented maximum SID size.
    unsafe {
        CreateWellKnownSid(
            WinLocalSystemSid,
            None,
            Some(PSID(sid.as_mut_ptr().cast())),
            &mut cb,
        )
    }
    .context("CreateWellKnownSid(S-1-5-18)")?;
    let ea = EXPLICIT_ACCESS_W {
        grfAccessPermissions: KEY_ALL_ACCESS.0,
        grfAccessMode: GRANT_ACCESS,
        grfInheritance: CONTAINER_INHERIT_ACE,
        Trustee: TRUSTEE_W {
            pMultipleTrustee: std::ptr::null_mut(),
            MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_USER,
            ptstrName: PWSTR(sid.as_mut_ptr().cast()),
        },
    };
    let mut new_dacl: *mut ACL = std::ptr::null_mut();
    // SAFETY: one live entry whose SID buffer outlives the call; old_dacl is the (possibly
    // null) DACL GetSecurityInfo returned, still owned by `sd`.
    unsafe {
        SetEntriesInAclW(
            Some(&[ea]),
            (!old_dacl.is_null()).then_some(old_dacl as *const ACL),
            &mut new_dacl,
        )
    }
    .ok()
    .context("SetEntriesInAclW")?;
    // SAFETY: `new_dacl` is the ACL SetEntriesInAclW just allocated; `Owned` frees it once.
    let _new_dacl = unsafe { Owned::new(HLOCAL(new_dacl.cast())) };
    // SAFETY: live handle; `new_dacl` stays allocated for the call.
    unsafe {
        SetSecurityInfo(
            handle,
            SE_REGISTRY_KEY,
            DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(new_dacl),
            None,
        )
    }
    .ok()
    .context("SetSecurityInfo(DACL)")
}

/// MMDevices hive from the id (`{0.0.1.…}` = capture, else render). Render is
/// the safe default: every non-capture id, and all pad endpoints, live there.
fn mmdev_path_for(endpoint_id: &str) -> &'static str {
    if endpoint_id.starts_with(CAPTURE_ENDPOINT_ID_PREFIX) {
        MMDEV_CAPTURE_PATH
    } else {
        MMDEV_RENDER_PATH
    }
}

/// Registry stamp route: repair the Properties ACL, then write serialized
/// values. Stored, possibly not served until an AEB restart — caller reads back.
fn registry_stamp(endpoint_id: &str, stamps: &[&Stamp]) -> Result<()> {
    use winreg::enums::HKEY_LOCAL_MACHINE;
    use winreg::RegKey;
    let guid = endpoint_guid_part(endpoint_id)?;
    // Hive follows direction. Hardcoding Render made the minted mic's capture
    // fallback stamp a non-existent `…\Render\{capture-guid}\Properties`.
    let path = format!(r"{}\{guid}\Properties", mmdev_path_for(endpoint_id));
    grant_system_full_control(&path)
        .with_context(|| format!("make {path} writable (registry stamp route)"))?;
    let key = RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey_with_flags(&path, KEY_QUERY_VALUE.0 | KEY_SET_VALUE.0)
        .with_context(|| format!("open {path} for writing"))?;
    for s in stamps {
        key.set_raw_value(reg_value_name(&s.key), &reg_registry_value(&s.value))
            .with_context(|| format!("write {} ({})", reg_value_name(&s.key), s.label))?;
    }
    Ok(())
}

/// Fresh property store reads back every stamp: served, not merely stored.
/// Any error is "not served" (feeds the needs-AEB-kick decision).
fn all_served(endpoint_id: &str, pad_index: u8) -> bool {
    stamps_served(endpoint_id, &active_stamps(pad_index))
}

/// Shared with the minted-audio provider.
pub fn stamps_served(endpoint_id: &str, stamps: &[Stamp]) -> bool {
    let Ok(dev) = open_mmdevice(endpoint_id) else {
        return false;
    };
    // SAFETY: read-only property store on a COM-initialized thread.
    let Ok(store) = (unsafe { dev.OpenPropertyStore(STGM_READ) }) else {
        return false;
    };
    stamps.iter().all(|s| stamp_served(&store, s))
}

/// One served property's raw blob (a format, say), or `None` when unreadable or not a blob.
pub fn served_blob(endpoint_id: &str, key: &PROPERTYKEY) -> Option<Vec<u8>> {
    let dev = open_mmdevice(endpoint_id).ok()?;
    // SAFETY: read-only property store on a COM-initialized thread.
    let store = unsafe { dev.OpenPropertyStore(STGM_READ) }.ok()?;
    // SAFETY: the key is a valid PROPERTYKEY; GetValue returns an owned variant that clears
    // on drop.
    let pv = unsafe { store.GetValue(key) }.ok()?;
    pv_bytes(&pv)
}

/// Idempotent pad-audio provision for one slot: reuse or create the devnode,
/// bind SSS, wait, stamp DualSense identity, undo a default-playback flip.
/// Host startup, not per session. COM thread; WASAPI objects never leave it.
pub fn ensure(pad_index: u8) -> Result<PadEndpoint> {
    anyhow::ensure!(pad_index < 8, "pad index out of range (0..=7): {pad_index}");
    wasapi::initialize_mta()
        .ok()
        .context("CoInitializeEx (MTA)")?;
    let prev_default = audio_control::default_render_id();
    let device_instance = match find_devnode(pad_index)? {
        Some(inst) => inst,
        None => create_devnode(pad_index)?,
    };
    let endpoint_id = match find_endpoint_for_devnode(&device_instance)? {
        Some(ep) => ep,
        None => {
            install_sss_driver().context("bind the Steam Streaming Speakers driver")?;
            wait_for_endpoint(&device_instance, wasapi::Direction::Render, ENDPOINT_WAIT)?
        }
    };
    // Stamp until it stays. On a fresh endpoint the write lands, an immediate
    // check reports served, then AEB reverts the three format keys. Check after
    // STAMP_SETTLE. A false `needs_aeb_kick` restarts the machine audio stack
    // on every host start.
    let mut served = false;
    for attempt in 0..STAMP_ATTEMPTS {
        stamp_endpoint(&endpoint_id, pad_index)
            .with_context(|| format!("stamp pad endpoint {endpoint_id}"))?;
        thread::sleep(STAMP_SETTLE);
        served = all_served(&endpoint_id, pad_index);
        if served {
            if attempt > 0 {
                tracing::debug!(
                    pad = pad_index,
                    attempt = attempt + 1,
                    "pad endpoint stamps held after a re-pass"
                );
            }
            break;
        }
    }
    // A freshly registered render endpoint can grab the default. A pad speaker
    // as default swallows desktop audio — put the previous one back.
    if audio_control::default_render_id().as_deref() == Some(endpoint_id.as_str())
        && prev_default.as_deref() != Some(endpoint_id.as_str())
    {
        match &prev_default {
            Some(prev) => match audio_control::set_default_endpoint(prev) {
                Ok(()) => tracing::info!(pad = pad_index,
                    "default playback had moved to the new pad endpoint — restored the previous default"),
                Err(e) => tracing::warn!(pad = pad_index, error = %format!("{e:#}"),
                    "default playback moved to the new pad endpoint and could not be restored"),
            },
            None => tracing::warn!(pad = pad_index,
                "default playback moved to the new pad endpoint and no previous default is known"),
        }
    }
    Ok(PadEndpoint {
        endpoint_id,
        device_instance,
        pad_index,
        needs_aeb_kick: !served,
    })
}

/// Best-effort teardown (`pnputil /remove-device`). Tests and the
/// `pad-endpoint remove` hatch only; endpoints are persistent.
pub fn remove(pe: &PadEndpoint) {
    match pf_paths::remove_device(&pe.device_instance) {
        Ok(()) => tracing::info!(devnode = %pe.device_instance, "pad-audio devnode removed"),
        Err(e) => tracing::warn!(devnode = %pe.device_instance, error = %format!("{e:#}"),
            "pad-audio devnode not removed"),
    }
}

/// Locate, never create. `endpoint_id` is empty when the devnode exists but
/// the endpoint never registered.
pub fn find(pad_index: u8) -> Result<Option<PadEndpoint>> {
    wasapi::initialize_mta()
        .ok()
        .context("CoInitializeEx (MTA)")?;
    let Some(device_instance) = find_devnode(pad_index)? else {
        return Ok(None);
    };
    let endpoint_id = find_endpoint_for_devnode(&device_instance)?.unwrap_or_default();
    let needs_aeb_kick = endpoint_id.is_empty() || !all_served(&endpoint_id, pad_index);
    Ok(Some(PadEndpoint {
        endpoint_id,
        device_instance,
        pad_index,
        needs_aeb_kick,
    }))
}

pub fn print_status(pad_index: u8) -> Result<()> {
    use winreg::enums::HKEY_LOCAL_MACHINE;
    use winreg::RegKey;
    wasapi::initialize_mta()
        .ok()
        .context("CoInitializeEx (MTA)")?;
    let Some(inst) = find_devnode(pad_index)? else {
        println!("pad {pad_index}: no pad-audio devnode");
        return Ok(());
    };
    println!("pad {pad_index}: devnode {inst}");
    let Some(ep) = find_endpoint_for_devnode(&inst)? else {
        println!("  endpoint: NONE (driver not installed, or the endpoint never registered)");
        return Ok(());
    };
    println!("  endpoint: {ep}");
    let props = RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey(format!(
            r"{MMDEV_RENDER_PATH}\{}\Properties",
            endpoint_guid_part(&ep)?
        ))
        .context("open the endpoint's Properties key")?;
    let dev = open_mmdevice(&ep)?;
    // SAFETY: read-only property store on the MTA-initialized current thread.
    let store = unsafe { dev.OpenPropertyStore(STGM_READ) }.context("OpenPropertyStore")?;
    let mut all = true;
    for s in active_stamps(pad_index) {
        let stored = match &s.value {
            StampValue::Str(v) => props
                .get_value::<String, _>(reg_value_name(&s.key))
                .map(|got| got == *v)
                .unwrap_or(false),
            other => props
                .get_raw_value(reg_value_name(&s.key))
                .map(|rv| rv.bytes == reg_registry_value(other).bytes)
                .unwrap_or(false),
        };
        let served = stamp_served(&store, &s);
        all &= served;
        println!("  {:<14} stored={stored} served={served}", s.label);
    }
    println!("  needs_aeb_kick={}", !all);
    Ok(())
}

/// Is this render endpoint a pad speaker (never a mic target, loopback
/// source, or default)? Registry-only, callable off COM threads. Either:
/// the stored ContainerId is PFDS (`Data1 == "PFDS"`), or the owning
/// devnode carries `PunktfunkPadIndex` (or pre-install, the creation
/// DeviceDesc). Positives are cached; negatives are recomputed because an
/// endpoint seen before `ensure()` stamped it must flip on the next pass.
pub fn is_pad_render_endpoint(endpoint_id: &str) -> bool {
    static KNOWN: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let known = KNOWN.get_or_init(|| Mutex::new(HashSet::new()));
    if known.lock().unwrap().contains(endpoint_id) {
        return true;
    }
    let is_pad = compute_is_pad_endpoint(endpoint_id);
    if is_pad {
        known.lock().unwrap().insert(endpoint_id.to_string());
    }
    is_pad
}

fn compute_is_pad_endpoint(endpoint_id: &str) -> bool {
    use winreg::enums::HKEY_LOCAL_MACHINE;
    use winreg::RegKey;
    let Ok(guid) = endpoint_guid_part(endpoint_id) else {
        return false;
    };
    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    let Ok(props) = hklm.open_subkey(format!(r"{MMDEV_RENDER_PATH}\{guid}\Properties")) else {
        return false;
    };
    // Stamped container: serialized VT_CLSID whose Data1 spells "PFDS".
    if let Ok(v) = props.get_raw_value(reg_value_name(&PKEY_CONTAINER_ID)) {
        if v.bytes.len() >= 12 && v.bytes[0] == 0x48 && v.bytes[8..12] == PFDS_TAG.to_le_bytes() {
            return true;
        }
    }
    // Created but not yet stamped: the owning devnode carries our marker.
    let Ok(link) = props.get_value::<String, _>(reg_value_name(&PKEY_ENDPOINT_DEVNODE)) else {
        return false;
    };
    let Some(inst) = link.strip_prefix("{1}.") else {
        return false;
    };
    let enum_key = format!(r"SYSTEM\CurrentControlSet\Enum\{inst}");
    if hklm
        .open_subkey(format!(r"{enum_key}\Device Parameters"))
        .and_then(|k| k.get_raw_value(PAD_INDEX_VALUE))
        .is_ok()
    {
        return true;
    }
    hklm.open_subkey(&enum_key)
        .and_then(|k| k.get_value::<String, _>("DeviceDesc"))
        .map(|d| d == DEVNODE_DESC)
        .unwrap_or(false)
}

fn pad_audio_enabled() -> bool {
    pf_host_config::row_bool("PUNKTFUNK_PAD_AUDIO")
}

fn pad_audio_slots() -> u8 {
    std::env::var("PUNKTFUNK_PAD_AUDIO_SLOTS")
        .ok()
        .and_then(|s| s.parse::<u8>().ok())
        .unwrap_or(1)
        .clamp(1, 4)
}

/// Set only on success. Failure leaves it unset so [`ensure_provisioned`] can retry.
static PROVISIONED: OnceLock<Arc<Vec<PadEndpoint>>> = OnceLock::new();

/// Guards the retry in [`ensure_provisioned`] against a second COM worker.
static PROVISIONING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Pads whose endpoint still refuses the streamer's open after [`validate`]'s ladder. The
/// streamer does not start on one; diagnostics names it.
static REFUSED: [std::sync::atomic::AtomicBool; 8] =
    [const { std::sync::atomic::AtomicBool::new(false) }; 8];

pub fn refuses_format(pad_index: u8) -> bool {
    REFUSED
        .get(pad_index as usize)
        .is_some_and(|b| b.load(std::sync::atomic::Ordering::Relaxed))
}

fn set_refused(pad_index: u8, refused: bool) {
    if let Some(b) = REFUSED.get(pad_index as usize) {
        b.store(refused, std::sync::atomic::Ordering::Relaxed);
    }
}

/// A shown endpoint re-activates on audiosrv's own clock; a fresh mint throws
/// DEVICE_INVALIDATED for ~20 s while its stamps settle.
const OPEN_SETTLE: Duration = Duration::from_secs(4);
const MINT_SETTLE: Duration = Duration::from_secs(25);

/// Retry the streamer's open until it passes or `budget` runs out. A refused format gets
/// 2 s at most: it is the endpoint's graph, and waiting does not change a graph.
fn opens_within(endpoint_id: &str, budget: Duration) -> Result<()> {
    let start = Instant::now();
    loop {
        let err = match super::pad_capture::probe_open(endpoint_id) {
            Ok(()) => return Ok(()),
            Err(e) => e,
        };
        let cap = if super::pad_capture::needs_reshape(&err) {
            budget.min(Duration::from_secs(2))
        } else {
            budget
        };
        if start.elapsed() >= cap {
            return Err(err);
        }
        thread::sleep(Duration::from_millis(500));
    }
}

/// The policy API refused every 4-channel format: the driver has no such mode, and no
/// re-mint of the devnode changes what the driver can do.
#[derive(Debug)]
struct DriverLacksMode;

impl std::fmt::Display for DriverLacksMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the pad endpoint's driver has no 4-channel mode")
    }
}

impl std::error::Error for DriverLacksMode {}

/// Ask the driver for the pad's 4-channel mode through the policy API, then probe again. The
/// stamps bypass the driver; this write does not, so its refusal is the driver's own answer.
fn reshape_and_probe(pe: &PadEndpoint) -> Result<()> {
    // The stamps' own PCM16 first: a driver that takes it leaves the stored device format
    // byte-identical to the stamp, so the next start re-stamps nothing.
    let samples = [
        (16, 16, wasapi::SampleType::Int),
        (32, 32, wasapi::SampleType::Float),
        (32, 24, wasapi::SampleType::Int),
        (24, 24, wasapi::SampleType::Int),
        (32, 32, wasapi::SampleType::Int),
    ];
    match audio_control::set_endpoint_format(
        &pe.endpoint_id,
        super::pad_capture::PAD_CHANNELS as u16,
        crate::SAMPLE_RATE,
        &[super::pad_capture::PAD_CHANNEL_MASK],
        &samples,
    ) {
        Ok(()) => {
            tracing::info!(
                pad = pe.pad_index,
                "pad endpoint set to 4 channels through the policy API"
            );
            opens_within(&pe.endpoint_id, OPEN_SETTLE)
        }
        Err(e) => Err(e.context(DriverLacksMode)),
    }
}

/// Remove the devnode and wait until its endpoint is deregistered too. pnputil returns
/// before either is gone, and the next mint reuses the instance path, so an `ensure` that
/// runs early finds the old endpoint under the new devnode and stamps a dying object.
fn remove_and_wait(pe: &PadEndpoint) {
    remove(pe);
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(10) {
        let devnode = find_devnode(pe.pad_index).ok().flatten().is_some();
        let endpoint = find_endpoint_for_devnode(&pe.device_instance)
            .ok()
            .flatten()
            .is_some_and(|id| id == pe.endpoint_id);
        if !devnode && !endpoint {
            return;
        }
        thread::sleep(Duration::from_millis(250));
    }
    tracing::warn!(pad = pe.pad_index, endpoint = %pe.endpoint_id,
        "the removed pad endpoint is still registered after 10 s — minting beside it");
}

/// Record and log one endpoint's verdict.
fn record_verdict(pe: &PadEndpoint, outcome: &Result<()>) {
    match outcome {
        Ok(()) => tracing::info!(pad = pe.pad_index, endpoint = %pe.endpoint_id,
            "pad endpoint takes the streamer's open"),
        Err(e) => tracing::warn!(pad = pe.pad_index, endpoint = %pe.endpoint_id,
            error = %format!("{e:#}"),
            "pad endpoint refuses the streamer's open — no controller audio on this pad until \
             it is repaired (pad_audio diagnostics row)"),
    }
    set_refused(pe.pad_index, outcome.is_err());
}

/// The verdict ladder for one endpoint: probe with the streamer's own open; on a refused
/// format set the 4-channel mode through the policy API and probe again; then, if allowed,
/// re-mint the devnode once and probe again. Shows the endpoint for the probes and hides it
/// after. Returns the endpoint, re-minted or not; the verdict lands in [`refuses_format`].
pub fn validate(mut pe: PadEndpoint, may_remint: bool) -> PadEndpoint {
    let idx = pe.pad_index;
    if pe.endpoint_id.is_empty() {
        return pe;
    }
    set_visibility(&pe.endpoint_id, idx, true);
    let mut outcome = opens_within(&pe.endpoint_id, OPEN_SETTLE);
    if matches!(&outcome, Err(e) if super::pad_capture::needs_reshape(e)) {
        outcome = reshape_and_probe(&pe);
    }
    let driver_lacks_mode = outcome
        .as_ref()
        .is_err_and(|e| e.downcast_ref::<DriverLacksMode>().is_some());
    if outcome.is_err() && may_remint && !driver_lacks_mode {
        tracing::warn!(pad = idx, endpoint = %pe.endpoint_id,
            error = %format!("{:#}", outcome.as_ref().unwrap_err()),
            "pad endpoint does not open — re-minting its devnode");
        set_visibility(&pe.endpoint_id, idx, false);
        remove_and_wait(&pe);
        match ensure(idx) {
            Ok(fresh) => {
                pe = fresh;
                set_visibility(&pe.endpoint_id, idx, true);
                outcome = opens_within(&pe.endpoint_id, MINT_SETTLE);
                if matches!(&outcome, Err(e) if super::pad_capture::needs_reshape(e)) {
                    outcome = reshape_and_probe(&pe);
                }
            }
            Err(e) => outcome = Err(e.context("re-mint the pad endpoint")),
        }
    }
    record_verdict(&pe, &outcome);
    set_visibility(&pe.endpoint_id, idx, false);
    pe
}

/// The in-session half of [`validate`], on an endpoint the streamer has already shown: no
/// re-mint, since the streamer holds this endpoint id for its life.
pub fn repair_shown(pad_index: u8, endpoint_id: &str) {
    let Some(pe) = endpoint_for(pad_index).filter(|pe| pe.endpoint_id == endpoint_id) else {
        return;
    };
    let mut outcome = opens_within(endpoint_id, Duration::from_secs(1));
    if matches!(&outcome, Err(e) if super::pad_capture::needs_reshape(e)) {
        outcome = reshape_and_probe(&pe);
    }
    record_verdict(&pe, &outcome);
}

/// Re-mint the devnode by hand and run the ladder on the fresh endpoint: the `pad-endpoint
/// repair --remint` hatch, so the re-mint branch can be watched without a broken driver.
pub fn remint(pe: &PadEndpoint) -> Result<PadEndpoint> {
    let idx = pe.pad_index;
    remove_and_wait(pe);
    let fresh = ensure(idx).context("re-mint the pad endpoint")?;
    set_visibility(&fresh.endpoint_id, idx, true);
    let mut outcome = opens_within(&fresh.endpoint_id, MINT_SETTLE);
    if matches!(&outcome, Err(e) if super::pad_capture::needs_reshape(e)) {
        outcome = reshape_and_probe(&fresh);
    }
    record_verdict(&fresh, &outcome);
    set_visibility(&fresh.endpoint_id, idx, false);
    Ok(fresh)
}

/// What the `pad_audio` diagnostics row reports.
pub enum PadAudioHealth {
    Off,
    NotProvisioned,
    Ok {
        slots: usize,
    },
    /// `pad`'s endpoint refuses the streamer's open, or its stamps are not served.
    Refused {
        pad: u8,
    },
}

pub fn health() -> PadAudioHealth {
    if !pad_audio_enabled() {
        return PadAudioHealth::Off;
    }
    let Some(eps) = PROVISIONED.get() else {
        return PadAudioHealth::NotProvisioned;
    };
    match eps
        .iter()
        .find(|pe| pe.needs_aeb_kick || refuses_format(pe.pad_index))
    {
        Some(pe) => PadAudioHealth::Refused { pad: pe.pad_index },
        None => PadAudioHealth::Ok { slots: eps.len() },
    }
}

/// Host-startup pre-provision: COM worker `ensure()`s slots `0..N`, at most
/// one AEB+Audiosrv restart if a stamp is stored-but-not-served, then
/// publishes for [`endpoint_for`]. Failure logs once and leaves the feature
/// off — the pad still works, without audio.
///
/// `startup` gates that restart. It bounces Audiosrv and AudioEndpointBuilder,
/// which cuts every stream on the box, so only the pre-session call may do it —
/// [`ensure_provisioned`] re-enters this from a live session.
pub fn provision_at_startup(startup: bool) {
    if !pad_audio_enabled() {
        tracing::info!("pad audio disabled (PUNKTFUNK_PAD_AUDIO=0)");
        // Previous-run endpoints persist and stay visible; idle libScePad
        // titles stall on them. Turning the feature off must park leftovers.
        hide_leftover_endpoints();
        return;
    }
    if PROVISIONED.get().is_some() {
        return;
    }
    // One attempt at a time; without this the retry could stack COM workers.
    if PROVISIONING.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    let slots = pad_audio_slots();
    let spawned = thread::Builder::new()
        .name("punktfunk-pad-audio".into())
        .spawn(move || {
            let mut eps: Vec<PadEndpoint> = Vec::new();
            for idx in 0..slots {
                match ensure(idx) {
                    Ok(pe) => {
                        tracing::info!(pad = idx, endpoint = %pe.endpoint_id,
                            needs_aeb_kick = pe.needs_aeb_kick, "pad-audio endpoint ready");
                        eps.push(pe);
                    }
                    Err(e) => {
                        tracing::warn!(pad = idx, error = %format!("{e:#}"),
                            "pad-audio endpoint provisioning failed — pad audio unavailable \
                             (pads still work, without a pad speaker)");
                        break;
                    }
                }
            }
            // The restart cuts every stream on the box, so a mid-session retry leaves the
            // stamps stored-but-not-served until the next host start instead.
            if startup && eps.iter().any(|p| p.needs_aeb_kick) {
                match restart_audio_endpoint_services() {
                    Ok(()) => {
                        for pe in &mut eps {
                            pe.needs_aeb_kick = !all_served(&pe.endpoint_id, pe.pad_index);
                            if pe.needs_aeb_kick {
                                tracing::warn!(pad = pe.pad_index, endpoint = %pe.endpoint_id,
                                    "pad endpoint stamps still not served after the audio-stack \
                                     restart");
                            }
                        }
                    }
                    Err(e) => tracing::warn!(error = %format!("{e:#}"),
                        "could not restart the audio stack for the pad endpoints — stamps stay \
                         stored-but-not-served until the next reboot"),
                }
            }
            // Prove each endpoint takes the streamer's open before parking it: a refused
            // format is repaired or reported here, not two seconds into a session. Each
            // comes back hidden: a visible idle DualSense-named speaker makes libScePad
            // titles stall on an unserviced endpoint.
            let eps: Vec<PadEndpoint> = eps.into_iter().map(|pe| validate(pe, true)).collect();
            // Latch only if something provisioned. Storing an empty vec on the
            // first error made OnceLock disable pad audio for the process life.
            if eps.is_empty() {
                tracing::warn!(
                    "pad-audio provisioning produced no endpoints — leaving it unlatched so the \
                     next session retries rather than disabling pad audio for this process"
                );
            } else {
                let _ = PROVISIONED.set(Arc::new(eps));
            }
            PROVISIONING.store(false, std::sync::atomic::Ordering::SeqCst);
        });
    if let Err(e) = spawned {
        PROVISIONING.store(false, std::sync::atomic::Ordering::SeqCst);
        tracing::warn!(error = %e, "pad-audio provisioning thread not spawned");
    }
}

#[allow(dead_code)]
pub fn provisioned_endpoints() -> Option<Arc<Vec<PadEndpoint>>> {
    PROVISIONED.get().cloned()
}

/// Retry if startup produced nothing. Cheap and idempotent: a successful latch
/// returns immediately; `PROVISIONING` keeps concurrent askers to one worker.
/// Recovers on the next connect, not the next reboot.
pub fn ensure_provisioned() {
    if PROVISIONED.get().is_none() {
        provision_at_startup(false);
    }
}

/// Show or hide a pad endpoint. Hidden = `DEVICE_STATE_DISABLED`; the
/// devnode, driver, and DualSense stamps stay, but the endpoint vanishes
/// from ACTIVE enumeration and cannot be opened. Idle libScePad titles
/// treat a visible DualSense-named speaker as a real pad and stall on it.
/// Flips raise an endpoint-state notification, not PnP.
pub fn set_visibility(endpoint_id: &str, pad_index: u8, visible: bool) {
    match audio_control::set_endpoint_visibility(endpoint_id, visible) {
        Ok(()) => tracing::info!(pad = pad_index, endpoint = %endpoint_id,
            state = if visible { "shown (client pad attached)" } else { "hidden (no pad attached)" },
            "pad-audio endpoint visibility"),
        Err(e) => tracing::warn!(pad = pad_index, endpoint = %endpoint_id, visible,
            error = %format!("{e:#}"),
            "pad-audio endpoint visibility change failed — an idle visible pad speaker can \
             stall libScePad titles (disable it in mmsys.cpl as a manual fallback)"),
    }
}

/// Hide leftovers on the `PUNKTFUNK_PAD_AUDIO=0` path, where the worker never
/// runs but persisted endpoints would stay visible and stall idle libScePad titles.
fn hide_leftover_endpoints() {
    let spawned = thread::Builder::new()
        .name("punktfunk-pad-audio-hide".into())
        .spawn(|| {
            if wasapi::initialize_mta().ok().is_err() {
                return;
            }
            for idx in 0..4u8 {
                match find(idx) {
                    Ok(Some(pe)) if !pe.endpoint_id.is_empty() => {
                        set_visibility(&pe.endpoint_id, idx, false);
                    }
                    _ => {}
                }
            }
        });
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "pad-endpoint hide sweep not spawned");
    }
}

#[allow(dead_code)]
pub fn endpoint_for(pad_index: u8) -> Option<PadEndpoint> {
    PROVISIONED
        .get()?
        .iter()
        .find(|p| p.pad_index == pad_index)
        .cloned()
}

/// Restart AudioEndpointBuilder + Audiosrv so registry-routed stamps get served.
/// Dependent Audiosrv stops first, starts last.
fn restart_audio_endpoint_services() -> Result<()> {
    use windows_service::service::{Service, ServiceAccess, ServiceState};
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

    fn stop_and_wait(svc: &Service, name: &str) -> Result<()> {
        let _ = svc.stop(); // ERROR_SERVICE_NOT_ACTIVE: already down
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let state = svc
                .query_status()
                .with_context(|| format!("query {name} status"))?;
            if state.current_state == ServiceState::Stopped {
                return Ok(());
            }
            if Instant::now() >= deadline {
                bail!("{name} did not stop within 20 s");
            }
            thread::sleep(Duration::from_millis(250));
        }
    }

    tracing::info!(
        "restarting AudioEndpointBuilder + Audiosrv once so the pad endpoints serve their \
         stamped identity (host startup, before any session)"
    );
    let mgr = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .context("open Service Control Manager")?;
    let access = ServiceAccess::STOP | ServiceAccess::START | ServiceAccess::QUERY_STATUS;
    let audiosrv = mgr
        .open_service("Audiosrv", access)
        .context("open Audiosrv")?;
    let aeb = mgr
        .open_service("AudioEndpointBuilder", access)
        .context("open AudioEndpointBuilder")?;
    stop_and_wait(&audiosrv, "Audiosrv")?;
    let no_args: &[&std::ffi::OsStr] = &[];
    let cycle = || -> Result<()> {
        stop_and_wait(&aeb, "AudioEndpointBuilder")?;
        aeb.start(no_args).context("start AudioEndpointBuilder")?;
        audiosrv.start(no_args).context("start Audiosrv")
    };
    // Audiosrv is down from here: a failure must not leave the box without sound.
    if let Err(e) = cycle() {
        let _ = aeb.start(no_args);
        let _ = audiosrv.start(no_args);
        return Err(e);
    }
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let state = audiosrv.query_status().context("query Audiosrv status")?;
        if state.current_state == ServiceState::Running {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("Audiosrv did not come back within 20 s");
        }
        thread::sleep(Duration::from_millis(250));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // The blobs below encode the pad layout the capturer reads back.
    use super::super::pad_capture::PAD_CHANNEL_MASK;

    #[test]
    fn registry_stamp_hive_follows_the_endpoint_direction() {
        assert_eq!(
            mmdev_path_for("{0.0.1.00000000}.{2753f927-2093-4ab4-aa90-9d880e959128}"),
            MMDEV_CAPTURE_PATH,
            "the minted microphone's capture endpoint records under Capture"
        );
        assert_eq!(
            mmdev_path_for("{0.0.0.00000000}.{5da9b5c9-8a10-4b54-8cf6-ce02b8354f16}"),
            MMDEV_RENDER_PATH,
        );
        // Unrecognised ids keep Render rather than inventing a hive.
        assert_eq!(mmdev_path_for("nonsense"), MMDEV_RENDER_PATH);
    }

    #[test]
    fn container_registry_blob_matches_measured() {
        let v = reg_registry_value(&StampValue::Container(pfds_container_guid(0)));
        let expect: Vec<u8> = vec![
            0x48, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, // VT_CLSID header
            0x53, 0x44, 0x46, 0x50, // "PFDS" little-endian Data1
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];
        assert_eq!(v.bytes.as_ref(), expect.as_slice());
        for idx in [1u8, 3, 7] {
            let v = reg_registry_value(&StampValue::Container(pfds_container_guid(idx)));
            assert_eq!(v.bytes.len(), 24);
            assert_eq!(v.bytes[23], idx, "byte 23 must be the pad index");
        }
    }

    #[test]
    fn wfx_blobs_are_4ch_48k() {
        for (wfx, bits) in [(&WFX_PCM16_4CH_48K, 16u16), (&WFX_F32_4CH_48K, 32u16)] {
            assert_eq!(wfx.len(), 40);
            let ch = u16::from_le_bytes([wfx[2], wfx[3]]);
            let rate = u32::from_le_bytes([wfx[4], wfx[5], wfx[6], wfx[7]]);
            let byte_rate = u32::from_le_bytes([wfx[8], wfx[9], wfx[10], wfx[11]]);
            let align = u16::from_le_bytes([wfx[12], wfx[13]]);
            let b = u16::from_le_bytes([wfx[14], wfx[15]]);
            let mask = u32::from_le_bytes([wfx[20], wfx[21], wfx[22], wfx[23]]);
            assert_eq!(ch, 4);
            assert_eq!(rate, 48_000);
            assert_eq!(b, bits);
            assert_eq!(align as u32, ch as u32 * bits as u32 / 8);
            assert_eq!(byte_rate, rate * align as u32);
            assert_eq!(mask, PAD_CHANNEL_MASK);
        }
        // Serialized registry shape adds the 8-byte VT_BLOB header.
        let v = reg_registry_value(&StampValue::Format(&WFX_F32_4CH_48K));
        assert_eq!(v.bytes.len(), 48);
        assert_eq!(&v.bytes[..8], &[0x41, 0, 0, 0, 1, 0, 0, 0]);
    }

    #[test]
    fn reg_value_names() {
        assert_eq!(
            reg_value_name(&PKEY_CONTAINER_ID),
            "{8c7ed206-3f8a-4827-b3ab-ae9e1faefc6c},2"
        );
        assert_eq!(
            reg_value_name(&PKEY_ENDPOINT_DEVNODE),
            "{b3f8fa53-0004-438e-9003-51a46e139bfc},2"
        );
        assert_eq!(
            reg_value_name(&PKEY_ENDPOINT_DEVICE_NAME),
            "{b3f8fa53-0004-438e-9003-51a46e139bfc},6"
        );
    }

    #[test]
    fn endpoint_guid_extraction() {
        let id = "{0.0.0.00000000}.{aeb07c72-0f2b-4d3c-9a08-2b4a01234567}";
        assert_eq!(
            endpoint_guid_part(id).unwrap(),
            "{aeb07c72-0f2b-4d3c-9a08-2b4a01234567}"
        );
        assert!(endpoint_guid_part("bogus").is_err());
    }

    /// String stamps stay REG_SZ (UTF-16LE + NUL), not serialized blobs.
    #[test]
    fn string_stamp_is_reg_sz() {
        let v = reg_registry_value(&StampValue::Str("Wireless Controller"));
        assert_eq!(v.vtype, winreg::enums::REG_SZ);
        assert_eq!(v.bytes.len(), ("Wireless Controller".len() + 1) * 2);
        assert_eq!(&v.bytes[..2], &[b'W', 0]);
    }
}
