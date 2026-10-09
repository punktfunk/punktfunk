//! Wired DualSense output on Windows: match the pad's HID container to its four-channel
//! WASAPI render endpoint and play the mixed stream there. `mod.rs` builds this file as
//! `usb` on Windows.

use super::{
    container_guid_from_blob, hid_instance_from_interface_path, pick_pad_endpoint,
    EndpointCandidate, TIER_A_PADS,
};
use punktfunk_core::audio::pad_mix::PAD_CHANNELS;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

pub(crate) fn wired_audio_sibling(hid_path: Option<&str>) -> bool {
    hid_path.is_some_and(|p| correlate_pad_endpoint(p).is_ok())
}

/// First registered USB pad's HID path — v1 renders one DualSense.
fn first_tier_a_hid_path() -> Option<String> {
    TIER_A_PADS
        .lock()
        .unwrap()
        .iter()
        .find(|p| !p.bluetooth)
        .and_then(|p| p.hid_path.clone())
}

/// HID interface path → instance id → Enum `ContainerID` → 4-ch eRender endpoint with matching
/// `PKEY_Device_ContainerId`. Registry for property reads (MMDevices ACL denies writes, not reads).
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

fn hid_container_id(instance: &str) -> anyhow::Result<String> {
    use anyhow::Context;
    let key = winreg::RegKey::predef(winreg::enums::HKEY_LOCAL_MACHINE)
        .open_subkey(format!(r"SYSTEM\CurrentControlSet\Enum\{instance}"))
        .with_context(|| format!(r"open Enum\{instance}"))?;
    key.get_value::<String, _>("ContainerID")
        .context("read ContainerID")
}

/// Active eRender endpoints. Own MTA thread (caller may be STA); one broken endpoint must not hide the rest.
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

pub(super) struct PadOut {
    pcm_tx: std::sync::mpsc::SyncSender<Vec<f32>>,
    recycle_rx: std::sync::mpsc::Receiver<Vec<f32>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl PadOut {
    /// Shared event-driven render stream (`audio_wasapi::render_thread` shape).
    pub(super) fn open() -> anyhow::Result<PadOut> {
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

    pub(super) fn take_buffer(&self) -> Vec<f32> {
        self.recycle_rx.try_recv().unwrap_or_default()
    }

    pub(super) fn push(&self, pcm: Vec<f32>) {
        let _ = self.pcm_tx.try_send(pcm); // never block the renderer; drops are concealed
    }

    pub(super) fn finished(&self) -> bool {
        self.thread.as_ref().is_none_or(|t| t.is_finished())
    }
}

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
                if silent_waits >= crate::audio::EVENT_SILENT_WAITS {
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
