//! The SetupAPI and PROPVARIANT layer under the Windows audio devnodes.
//!
//! Creating a root-enumerated media devnode, reading and writing its device parameters, and
//! marshalling `PROPVARIANT`s for the endpoint property store are FFI plumbing, not policy.
//! `pad_endpoint`, `minted`, `audio_probe` and `devnode_cleanup` all reach for it, and only one
//! of them provisions pads — which is why it is not in a file named for that.
//!
//! Every `pv_*` constructor returns a [`ManuallyDrop`]: the caller hands the variant to a
//! property store that takes the payload, so dropping it here would free bytes the store
//! still points at.

use anyhow::{anyhow, Context, Result};
use std::mem::ManuallyDrop;
use windows::core::{w, Owned, GUID, HSTRING, PCWSTR, PWSTR};
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    SetupDiCreateDevRegKeyW, SetupDiCreateDeviceInfoList, SetupDiCreateDeviceInfoW,
    SetupDiEnumDeviceInfo, SetupDiGetClassDevsW, SetupDiGetDeviceInstanceIdW,
    SetupDiGetDevicePropertyW, SetupDiGetDeviceRegistryPropertyW, SetupDiOpenDevRegKey,
    SetupDiRegisterDeviceInfo, SetupDiSetDeviceRegistryPropertyW,
    UpdateDriverForPlugAndPlayDevicesW, DICD_GENERATE_ID, DICS_FLAG_GLOBAL, DIREG_DEV,
    GUID_DEVCLASS_MEDIA, HDEVINFO, SETUP_DI_GET_CLASS_DEVS_FLAGS, SPDRP_HARDWAREID,
    SP_DEVINFO_DATA, UPDATEDRIVERFORPLUGANDPLAYDEVICES_FLAGS,
};
use windows::Win32::Devices::Properties::{
    DEVPKEY_Device_DriverInfPath, DEVPROPTYPE, DEVPROP_TYPE_STRING,
};
use windows::Win32::System::Com::StructuredStorage::{
    PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0,
};
use windows::Win32::System::Com::BLOB;
use windows::Win32::System::Registry::{
    RegQueryValueExW, RegSetValueExW, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_DWORD, REG_VALUE_TYPE,
};
use windows::Win32::System::Variant::{VT_BLOB, VT_CLSID, VT_LPWSTR};

/// An owned NUL-terminated UTF-16 buffer, for a property value that borrows or copies the
/// bytes. A read-only `PCWSTR` argument takes an `HSTRING` instead.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// `VT_LPWSTR` borrowing `w`, which must outlive it and stay NUL-terminated.
pub fn pv_lpwstr(w: &[u16]) -> ManuallyDrop<PROPVARIANT> {
    ManuallyDrop::new(PROPVARIANT {
        Anonymous: PROPVARIANT_0 {
            Anonymous: ManuallyDrop::new(PROPVARIANT_0_0 {
                vt: VT_LPWSTR,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: PROPVARIANT_0_0_0 {
                    pwszVal: PWSTR(w.as_ptr().cast_mut()),
                },
            }),
        },
    })
}

/// `VT_CLSID` borrowing `g`, which must outlive it.
pub fn pv_clsid(g: &GUID) -> ManuallyDrop<PROPVARIANT> {
    ManuallyDrop::new(PROPVARIANT {
        Anonymous: PROPVARIANT_0 {
            Anonymous: ManuallyDrop::new(PROPVARIANT_0_0 {
                vt: VT_CLSID,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: PROPVARIANT_0_0_0 {
                    puuid: std::ptr::from_ref(g).cast_mut(),
                },
            }),
        },
    })
}

/// `VT_BLOB` borrowing `b`, which must outlive it.
pub fn pv_blob(b: &[u8]) -> ManuallyDrop<PROPVARIANT> {
    ManuallyDrop::new(PROPVARIANT {
        Anonymous: PROPVARIANT_0 {
            Anonymous: ManuallyDrop::new(PROPVARIANT_0_0 {
                vt: VT_BLOB,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: PROPVARIANT_0_0_0 {
                    blob: BLOB {
                        cbSize: b.len() as u32,
                        pBlobData: b.as_ptr().cast_mut(),
                    },
                },
            }),
        },
    })
}

pub fn pv_string(pv: &PROPVARIANT) -> Option<String> {
    // SAFETY: the variant is initialized (built by us or returned by GetValue); pwszVal is only
    // read when vt says VT_LPWSTR, in which case it points at the variant's NUL-terminated
    // string (or is null, which we check).
    unsafe {
        let inner = &pv.Anonymous.Anonymous;
        if inner.vt != VT_LPWSTR {
            return None;
        }
        let p = inner.Anonymous.pwszVal;
        if p.is_null() {
            return None;
        }
        p.to_string().ok()
    }
}

pub fn pv_guid(pv: &PROPVARIANT) -> Option<GUID> {
    // SAFETY: puuid is only dereferenced when vt == VT_CLSID and non-null.
    unsafe {
        let inner = &pv.Anonymous.Anonymous;
        if inner.vt != VT_CLSID {
            return None;
        }
        let p = inner.Anonymous.puuid;
        if p.is_null() {
            return None;
        }
        Some(*p)
    }
}

pub fn pv_bytes(pv: &PROPVARIANT) -> Option<Vec<u8>> {
    // SAFETY: the blob pointer/length pair is only read when vt == VT_BLOB and
    // the pointer is non-null; the variant owns cbSize bytes there.
    unsafe {
        let inner = &pv.Anonymous.Anonymous;
        if inner.vt != VT_BLOB {
            return None;
        }
        let b = &inner.Anonymous.blob;
        if b.pBlobData.is_null() {
            return None;
        }
        Some(std::slice::from_raw_parts(b.pBlobData, b.cbSize as usize).to_vec())
    }
}

/// A device-information set, destroyed on drop.
pub struct DevInfoSet(Owned<HDEVINFO>);

impl DevInfoSet {
    /// Adopts a set from `SetupDiGetClassDevsW` / `SetupDiCreateDeviceInfoList`.
    fn adopt(set: HDEVINFO) -> Self {
        // SAFETY: both producers hand the caller a set it alone owns and destroys once.
        Self(unsafe { Owned::new(set) })
    }

    /// Every element, in SetupAPI order, until the first `ERROR_NO_MORE_ITEMS`.
    pub fn iter(&self) -> impl Iterator<Item = SP_DEVINFO_DATA> + '_ {
        (0..).map_while(move |i| {
            let mut did = devinfo_data();
            // SAFETY: live set; `did` is a live out-param with cbSize set.
            unsafe { SetupDiEnumDeviceInfo(*self.0, i, &mut did) }
                .ok()
                .map(|()| did)
        })
    }
}

pub fn media_class_devs() -> Result<DevInfoSet> {
    // SAFETY: the class GUID is a static const; flags 0 (not DIGCF_PRESENT) so a created-but-
    // never-installed phantom from a previous run is still found and reused, not duplicated.
    let set = unsafe {
        SetupDiGetClassDevsW(
            Some(&GUID_DEVCLASS_MEDIA),
            PCWSTR::null(),
            None,
            SETUP_DI_GET_CLASS_DEVS_FLAGS(0),
        )
    }
    .context("SetupDiGetClassDevs(MEDIA)")?;
    Ok(DevInfoSet::adopt(set))
}

fn devinfo_data() -> SP_DEVINFO_DATA {
    SP_DEVINFO_DATA {
        cbSize: std::mem::size_of::<SP_DEVINFO_DATA>() as u32,
        ..Default::default()
    }
}

pub fn instance_id(set: &DevInfoSet, did: &SP_DEVINFO_DATA) -> Option<String> {
    let mut buf = [0u16; 200];
    // SAFETY: live devinfo set + element; the buffer length travels with the slice.
    unsafe { SetupDiGetDeviceInstanceIdW(*set.0, did, Some(&mut buf), None) }.ok()?;
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..len]))
}

pub fn devnode_multi_sz_prop(
    set: &DevInfoSet,
    did: &SP_DEVINFO_DATA,
    prop: windows::Win32::Devices::DeviceAndDriverInstallation::SETUP_DI_REGISTRY_PROPERTY,
) -> Vec<String> {
    let mut buf = vec![0u8; 4096];
    let mut req = 0u32;
    // SAFETY: live set + element; the output buffer length travels with the slice.
    if unsafe {
        SetupDiGetDeviceRegistryPropertyW(*set.0, did, prop, None, Some(&mut buf), Some(&mut req))
    }
    .is_err()
    {
        return Vec::new();
    }
    let units: Vec<u16> = buf[..(req as usize).min(buf.len())]
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    units
        .split(|&c| c == 0)
        .filter(|s| !s.is_empty())
        .map(String::from_utf16_lossy)
        .collect()
}

/// Installed-driver INF (`DEVPKEY_Device_DriverInfPath`). Absent if the driver never installed.
pub fn devnode_inf_path(set: &DevInfoSet, did: &SP_DEVINFO_DATA) -> Option<String> {
    let mut ty = DEVPROPTYPE(0);
    let mut buf = vec![0u8; 1024];
    let mut req = 0u32;
    // SAFETY: live set + element; the property key is a static const; the buffer length
    // travels with the slice.
    unsafe {
        SetupDiGetDevicePropertyW(
            *set.0,
            did,
            &DEVPKEY_Device_DriverInfPath,
            &mut ty,
            Some(&mut buf),
            Some(&mut req),
            0,
        )
    }
    .ok()?;
    if ty != DEVPROP_TYPE_STRING {
        return None;
    }
    let units: Vec<u16> = buf[..(req as usize).min(buf.len())]
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    let len = units.iter().position(|&c| c == 0).unwrap_or(units.len());
    (len > 0).then(|| String::from_utf16_lossy(&units[..len]))
}

/// REG_DWORD from `Device Parameters`. `None` = no key, no value, or wrong type — foreign.
pub fn read_devparam_dword(
    set: &DevInfoSet,
    did: &SP_DEVINFO_DATA,
    value_name: &str,
) -> Option<u32> {
    // SAFETY: live set + element; DIREG_DEV opens the devnode's Device Parameters key, which
    // `Owned` closes once.
    let hkey = unsafe {
        Owned::new(
            SetupDiOpenDevRegKey(
                *set.0,
                did,
                DICS_FLAG_GLOBAL.0,
                0,
                DIREG_DEV,
                KEY_QUERY_VALUE.0,
            )
            .ok()?,
        )
    };
    let name = HSTRING::from(value_name);
    let mut data = [0u8; 4];
    let mut len = data.len() as u32;
    let mut ty = REG_VALUE_TYPE(0);
    // SAFETY: the value name is NUL-terminated and outlives the call; data/len are live locals
    // sized together.
    let rc = unsafe {
        RegQueryValueExW(
            *hkey,
            PCWSTR(name.as_ptr()),
            None,
            Some(&mut ty),
            Some(data.as_mut_ptr()),
            Some(&mut len),
        )
    };
    (rc.is_ok() && ty == REG_DWORD && len == 4).then(|| u32::from_le_bytes(data))
}

/// Write side of [`read_devparam_dword`]; creates the key on a fresh devnode.
pub fn write_devparam_dword(
    set: &DevInfoSet,
    did: &mut SP_DEVINFO_DATA,
    value_name: &str,
    value: u32,
) -> Result<()> {
    // SAFETY: live set + element; DIREG_DEV opens the devnode's Device Parameters key.
    let opened = unsafe {
        SetupDiOpenDevRegKey(
            *set.0,
            did,
            DICS_FLAG_GLOBAL.0,
            0,
            DIREG_DEV,
            KEY_SET_VALUE.0,
        )
    };
    let hkey = match opened {
        Ok(k) => k,
        // SAFETY: same set + element; a fresh devnode has no Device Parameters key yet, so
        // create it (no INF association).
        Err(_) => unsafe {
            SetupDiCreateDevRegKeyW(
                *set.0,
                did,
                DICS_FLAG_GLOBAL.0,
                0,
                DIREG_DEV,
                None,
                PCWSTR::null(),
            )
        }
        .with_context(|| format!("create the Device Parameters key for {value_name}"))?,
    };
    // SAFETY: the key opened or created above is ours alone; `Owned` closes it once.
    let hkey = unsafe { Owned::new(hkey) };
    let name = HSTRING::from(value_name);
    // SAFETY: the value name is NUL-terminated and outlives the call; the DWORD bytes travel
    // with the slice.
    let rc = unsafe {
        RegSetValueExW(
            *hkey,
            PCWSTR(name.as_ptr()),
            None,
            REG_DWORD,
            Some(&value.to_le_bytes()),
        )
    };
    rc.ok().with_context(|| format!("write {value_name}"))
}

/// Create + register a MEDIA-class root devnode carrying `hwid`, then `mark`
/// writes the durable owner marker. DeviceDesc only survives until the INF
/// installs. Shared by the pad provisioner and the `audio-probe` devtest.
pub fn create_media_devnode(
    desc: &str,
    hwid: &str,
    mark: impl FnOnce(&DevInfoSet, &mut SP_DEVINFO_DATA) -> Result<()>,
) -> Result<String> {
    // SAFETY: the class GUID is a static const.
    let set = unsafe { SetupDiCreateDeviceInfoList(Some(&GUID_DEVCLASS_MEDIA), None) }
        .context("SetupDiCreateDeviceInfoList(MEDIA)")?;
    let set = DevInfoSet::adopt(set);
    let mut did = devinfo_data();
    let desc = HSTRING::from(desc);
    // SAFETY: name/class/description are live NUL-terminated buffers; DICD_GENERATE_ID makes
    // PnP mint the ROOT\MEDIA\00NN instance id; `did` receives the element.
    unsafe {
        SetupDiCreateDeviceInfoW(
            *set.0,
            w!("MEDIA"),
            &GUID_DEVCLASS_MEDIA,
            PCWSTR(desc.as_ptr()),
            None,
            DICD_GENERATE_ID,
            Some(&mut did),
        )
    }
    .context("SetupDiCreateDeviceInfo")?;
    let hwid: Vec<u8> = pf_win_display::multi_sz(&[hwid])
        .iter()
        .flat_map(|u| u.to_le_bytes())
        .collect();
    // SAFETY: live set + element; the multi-sz property bytes travel with the slice.
    unsafe { SetupDiSetDeviceRegistryPropertyW(*set.0, &mut did, SPDRP_HARDWAREID, Some(&hwid)) }
        .context("set SPDRP_HARDWAREID")?;
    // NOT SetupDiCallClassInstaller(DIF_REGISTERDEVICE): needs an interactive
    // window station and fails with 1459 from a service.
    // SAFETY: live set + element; no compare callback.
    unsafe { SetupDiRegisterDeviceInfo(*set.0, &mut did, 0, None, None, None) }
        .context("SetupDiRegisterDeviceInfo")?;
    mark(&set, &mut did)?;
    instance_id(&set, &did).context("read the new devnode's instance id")
}

/// Bind `inf` to every unbound devnode carrying `hwid`. Idempotent: nothing
/// needed an update is success. Shared with the `audio-probe` devtest.
pub fn bind_driver(hwid: &str, inf: &str) -> Result<()> {
    let inf_w = HSTRING::from(inf);
    let hwid_w = HSTRING::from(hwid);
    // SAFETY: both strings are NUL-terminated and outlive the call; a null parent HWND and no
    // reboot-required out-param are documented as accepted.
    let r = unsafe {
        UpdateDriverForPlugAndPlayDevicesW(
            None,
            PCWSTR(hwid_w.as_ptr()),
            PCWSTR(inf_w.as_ptr()),
            UPDATEDRIVERFORPLUGANDPLAYDEVICES_FLAGS(0),
            None,
        )
    };
    match r {
        Ok(()) => {
            tracing::info!(hwid = %hwid, inf = %inf, "bound the driver to the unbound devnode(s)");
            Ok(())
        }
        // ERROR_NO_MORE_ITEMS (0x80070103): every matching devnode already runs
        // this (or a better) driver — idempotent reissue, not a failure.
        Err(e) if e.code().0 as u32 == 0x8007_0103 => Ok(()),
        Err(e) => {
            Err(anyhow!(e)).with_context(|| format!("UpdateDriverForPlugAndPlayDevices({inf})"))
        }
    }
}
