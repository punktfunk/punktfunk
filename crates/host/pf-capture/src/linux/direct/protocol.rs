//! The protocols the direct capture speaks, one wayland-scanner module per XML, and the
//! [`State`] every callback writes: the globals, the session's constraints, the output's
//! colour description, and each frame's events.

use super::frame_loop::Event;
use anyhow::Result;
use pf_frame::HdrMeta;
use wayland_client::protocol::{wl_buffer, wl_callback, wl_output, wl_registry, wl_shm};
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum};

// One module per protocol XML: the scanner emits the same private helper names into each,
// so two `generate_interfaces!` in one module collide. `icc` names an interface from
// `src_proto`, so both its code and its interface table import that module.
pub mod src_proto {
    #![allow(clippy::too_many_arguments, missing_docs)]
    use wayland_client;
    use wayland_client::protocol::*;

    pub mod __interfaces {
        use wayland_client::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("protocols/ext-image-capture-source-v1.xml");
    }
    use self::__interfaces::*;

    wayland_scanner::generate_client_code!("protocols/ext-image-capture-source-v1.xml");
}

pub mod icc {
    #![allow(clippy::too_many_arguments, missing_docs)]
    use super::src_proto::*;
    use wayland_client;
    use wayland_client::protocol::*;

    pub mod __interfaces {
        use super::super::src_proto::__interfaces::*;
        use wayland_client::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("protocols/ext-image-copy-capture-v1.xml");
    }
    use self::__interfaces::*;

    wayland_scanner::generate_client_code!("protocols/ext-image-copy-capture-v1.xml");
}

pub mod dmabuf {
    #![allow(clippy::too_many_arguments, missing_docs)]
    use wayland_client;
    use wayland_client::protocol::*;

    pub mod __interfaces {
        use wayland_client::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("protocols/linux-dmabuf-v1.xml");
    }
    use self::__interfaces::*;

    wayland_scanner::generate_client_code!("protocols/linux-dmabuf-v1.xml");
}

pub mod cm {
    #![allow(clippy::too_many_arguments, missing_docs)]
    use wayland_client;
    use wayland_client::protocol::*;

    pub mod __interfaces {
        use wayland_client::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("protocols/color-management-v1.xml");
    }
    use self::__interfaces::*;

    wayland_scanner::generate_client_code!("protocols/color-management-v1.xml");
}

pub(super) use cm::wp_color_management_output_v1::{
    Event as CmOutputEvent, WpColorManagementOutputV1 as CmOutput,
};
pub(super) use cm::wp_color_manager_v1::{
    Primaries, TransferFunction, WpColorManagerV1 as ColorManager,
};
pub(super) use cm::wp_image_description_info_v1::{
    Event as InfoEvent, WpImageDescriptionInfoV1 as ImageInfo,
};
pub(super) use cm::wp_image_description_v1::{
    Event as DescEvent, WpImageDescriptionV1 as ImageDesc,
};
pub(super) use dmabuf::zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1 as BufferParams;
pub(super) use dmabuf::zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1 as LinuxDmabuf;
pub(super) use icc::ext_image_copy_capture_frame_v1::{
    Event as FrameEvent, ExtImageCopyCaptureFrameV1 as CaptureFrame,
};
pub(super) use icc::ext_image_copy_capture_manager_v1::ExtImageCopyCaptureManagerV1 as CaptureManager;
pub(super) use icc::ext_image_copy_capture_session_v1::{
    Event as SessionEvent, ExtImageCopyCaptureSessionV1 as CaptureSession,
};
pub(super) use src_proto::ext_image_capture_source_v1::ExtImageCaptureSourceV1 as CaptureSource;
pub(super) use src_proto::ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1 as OutputSourceManager;

/// Everything the Wayland callbacks mutate, in one place: `wayland-client` dispatches
/// against a single state value. The setup reads its flags; the frame loop steps on
/// [`Self::events`].
pub(super) struct State {
    // Globals, bound once.
    pub(super) source_mgr: Option<OutputSourceManager>,
    pub(super) capture_mgr: Option<CaptureManager>,
    pub(super) linux_dmabuf: Option<LinuxDmabuf>,
    pub(super) color_mgr: Option<ColorManager>,
    pub(super) outputs: Vec<(wl_output::WlOutput, Option<String>)>,

    // Session constraints, valid once `done` has landed.
    pub(super) size: Option<(u32, u32)>,
    pub(super) dmabuf_dev: Option<u64>,
    /// `(fourcc, modifiers)` in the order advertised.
    pub(super) dmabuf_formats: Vec<(u32, Vec<u64>)>,
    pub(super) constraints_done: bool,
    /// The session stopped. The setup's waits end on it; the frame loop reads it at its top.
    pub(super) stopped: bool,
    /// The compositor marked a `create_immed` buffer failed.
    pub(super) import_failed: bool,
    /// The last [`roundtrip_within`](super::session::roundtrip_within) sync came back.
    pub(super) synced: bool,

    // The output's image description, gathered event by event until `done`.
    pub(super) desc_ready: bool,
    pub(super) desc_failed: Option<String>,
    pub(super) color_pending: OutputColor,
    pub(super) color_done: bool,

    /// Compositor's presentation stamp for the frame in flight, ns. Its `ready` takes it.
    pub(super) presented_ns: Option<u64>,
    /// What the frame loop steps on, in arrival order. The loop drains it after each dispatch;
    /// events from the setup's round trips wait for the first.
    pub(super) events: Vec<Event>,
}

impl State {
    pub(super) fn output_named(&self, name: &str) -> Option<&wl_output::WlOutput> {
        self.outputs
            .iter()
            .find(|(_, n)| n.as_deref() == Some(name))
            .map(|(o, _)| o)
    }
}

/// An output's colour encoding as `wp_image_description_info_v1` reports it. Named
/// enums are kept as their wire values so a compositor's unknown entry still compares.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct OutputColor {
    /// `wp_color_manager_v1.primaries`; 0 = not named.
    pub primaries: u32,
    /// `wp_color_manager_v1.transfer_function`; 0 = not named.
    pub tf: u32,
    /// Mastering primaries, CIE xy × 1e6: red, green, blue, white.
    pub target_primaries: Option<[(i32, i32); 4]>,
    /// Mastering luminance: min cd/m² × 10000, max cd/m².
    pub target_luminance: Option<(u32, u32)>,
    pub max_cll: Option<u32>,
    pub max_fall: Option<u32>,
}

impl OutputColor {
    /// BT.2020 under the PQ curve — the one encoding the stream's HDR path carries.
    pub(super) fn is_hdr10(&self) -> bool {
        self.primaries == Primaries::Bt2020 as u32 && self.tf == TransferFunction::St2084Pq as u32
    }

    /// ST.2086 + CLL block for this description: [`pf_frame::hdr::generic_hdr10`] with each
    /// field the compositor sent written over it (Hyprland forwards the panel's EDID).
    /// CIE xy × 1e6 → 1/50000 units is a divide by 20.
    pub(super) fn hdr_meta(&self) -> HdrMeta {
        let mut m = pf_frame::hdr::generic_hdr10();
        if let Some([r, g, b, w]) = self.target_primaries {
            let xy = |(x, y): (i32, i32)| {
                [
                    (x / 20).clamp(0, 50_000) as u16,
                    (y / 20).clamp(0, 50_000) as u16,
                ]
            };
            m.display_primaries = [xy(g), xy(b), xy(r)];
            m.white_point = xy(w);
        }
        if let Some((min, max)) = self.target_luminance {
            m.min_display_mastering_luminance = min;
            m.max_display_mastering_luminance = max.saturating_mul(10_000);
        }
        let nits = |v: u32| v.min(u32::from(u16::MAX)) as u16;
        m.max_cll = self.max_cll.map_or(m.max_cll, nits);
        m.max_fall = self.max_fall.map_or(m.max_fall, nits);
        m
    }
}

fn wenum_raw<T: Into<u32>>(v: WEnum<T>) -> u32 {
    match v {
        WEnum::Value(v) => v.into(),
        WEnum::Unknown(v) => v,
    }
}

macro_rules! ignore_dispatch {
    ($($t:ty),+ $(,)?) => {$(
        impl Dispatch<$t, ()> for State {
            fn event(
                _: &mut Self,
                _: &$t,
                _: <$t as wayland_client::Proxy>::Event,
                _: &(),
                _: &Connection,
                _: &QueueHandle<Self>,
            ) {
            }
        }
    )+};
}
ignore_dispatch!(
    OutputSourceManager,
    CaptureManager,
    CaptureSource,
    LinuxDmabuf,
    ColorManager,
    wl_buffer::WlBuffer,
    wl_shm::WlShm,
);

impl Dispatch<BufferParams, ()> for State {
    fn event(
        st: &mut Self,
        _: &BufferParams,
        event: dmabuf::zwp_linux_buffer_params_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // `create_immed` never sends `created`.
        if let dmabuf::zwp_linux_buffer_params_v1::Event::Failed = event {
            st.import_failed = true;
        }
    }
}

impl Dispatch<wl_callback::WlCallback, ()> for State {
    fn event(
        st: &mut Self,
        _: &wl_callback::WlCallback,
        _: wl_callback::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        st.synced = true;
    }
}

impl Dispatch<CmOutput, ()> for State {
    fn event(
        st: &mut Self,
        _: &CmOutput,
        event: CmOutputEvent,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // The interface's only event.
        let CmOutputEvent::ImageDescriptionChanged = event;
        st.events.push(Event::ColourChanged);
    }
}

impl Dispatch<ImageDesc, ()> for State {
    fn event(
        st: &mut Self,
        _: &ImageDesc,
        event: DescEvent,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            DescEvent::Ready { .. } => st.desc_ready = true,
            DescEvent::Failed { msg, .. } => st.desc_failed = Some(msg),
            _ => {}
        }
    }
}

impl Dispatch<ImageInfo, ()> for State {
    fn event(
        st: &mut Self,
        _: &ImageInfo,
        event: InfoEvent,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let c = &mut st.color_pending;
        match event {
            InfoEvent::PrimariesNamed { primaries } => c.primaries = wenum_raw(primaries),
            InfoEvent::TfNamed { tf } => c.tf = wenum_raw(tf),
            InfoEvent::TargetPrimaries {
                r_x,
                r_y,
                g_x,
                g_y,
                b_x,
                b_y,
                w_x,
                w_y,
            } => c.target_primaries = Some([(r_x, r_y), (g_x, g_y), (b_x, b_y), (w_x, w_y)]),
            InfoEvent::TargetLuminance { min_lum, max_lum } => {
                c.target_luminance = Some((min_lum, max_lum));
            }
            InfoEvent::TargetMaxCll { max_cll } => c.max_cll = Some(max_cll),
            InfoEvent::TargetMaxFall { max_fall } => c.max_fall = Some(max_fall),
            InfoEvent::Done => st.color_done = true,
            _ => {}
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        st: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        else {
            return;
        };
        match interface.as_str() {
            "ext_output_image_capture_source_manager_v1" => {
                st.source_mgr = Some(registry.bind(name, 1.min(version), qh, ()));
            }
            "ext_image_copy_capture_manager_v1" => {
                st.capture_mgr = Some(registry.bind(name, 1.min(version), qh, ()));
            }
            "zwp_linux_dmabuf_v1" => {
                // `create_immed` needs v2; nothing above v3 is needed here.
                if version >= 3 {
                    st.linux_dmabuf = Some(registry.bind(name, 3, qh, ()));
                }
            }
            // v1 carries everything an output description needs; v2 adds nothing read here.
            "wp_color_manager_v1" => {
                st.color_mgr = Some(registry.bind(name, 1, qh, ()));
            }
            "wl_output" => {
                // v4 carries `name`, which is how the host addresses its own head.
                let o: wl_output::WlOutput = registry.bind(name, 4.min(version), qh, ());
                st.outputs.push((o, None));
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_output::WlOutput, ()> for State {
    fn event(
        st: &mut Self,
        out: &wl_output::WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = event {
            if let Some(slot) = st.outputs.iter_mut().find(|(o, _)| o == out) {
                slot.1 = Some(name);
            }
        }
    }
}

impl Dispatch<CaptureSession, ()> for State {
    fn event(
        st: &mut Self,
        _: &CaptureSession,
        event: SessionEvent,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            SessionEvent::BufferSize { width, height } => {
                if st.constraints_done && st.size != Some((width, height)) {
                    // A second constraint batch is a mode change: the pool no longer fits.
                    st.events.push(Event::Resized);
                }
                st.size = Some((width, height));
            }
            SessionEvent::DmabufDevice { device } => {
                let mut dev = [0u8; 8];
                let n = device.len().min(8);
                dev[..n].copy_from_slice(&device[..n]);
                st.dmabuf_dev = Some(u64::from_ne_bytes(dev));
            }
            SessionEvent::DmabufFormat { format, modifiers } => {
                let mods = modifiers
                    .chunks_exact(8)
                    .map(|c| u64::from_ne_bytes(c.try_into().unwrap_or([0; 8])))
                    .collect();
                st.dmabuf_formats.push((format, mods));
            }
            SessionEvent::Done => st.constraints_done = true,
            SessionEvent::Stopped => st.stopped = true,
            _ => {}
        }
    }
}

impl Dispatch<CaptureFrame, ()> for State {
    fn event(
        st: &mut Self,
        _: &CaptureFrame,
        event: FrameEvent,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            // `presentation_time` comes before `ready`, so the frame's stamp is in by now.
            FrameEvent::Ready => st.events.push(Event::Ready {
                presented: st.presented_ns.take(),
            }),
            FrameEvent::Failed { reason } => st.events.push(Event::Failed(match reason {
                WEnum::Value(v) => v as u32,
                WEnum::Unknown(v) => v,
            })),
            FrameEvent::PresentationTime {
                tv_sec_hi,
                tv_sec_lo,
                tv_nsec,
            } => {
                // Compositor values: an overflow is no stamp, and `wire_pts` falls back.
                let secs = ((tv_sec_hi as u64) << 32) | tv_sec_lo as u64;
                st.presented_ns = secs
                    .checked_mul(1_000_000_000)
                    .and_then(|ns| ns.checked_add(u64::from(tv_nsec)));
            }
            _ => {}
        }
    }
}

pub(super) fn new_state() -> Result<State> {
    Ok(State {
        source_mgr: None,
        capture_mgr: None,
        linux_dmabuf: None,
        color_mgr: None,
        outputs: Vec::new(),
        size: None,
        dmabuf_dev: None,
        dmabuf_formats: Vec::new(),
        constraints_done: false,
        stopped: false,
        import_failed: false,
        synced: false,
        desc_ready: false,
        desc_failed: None,
        color_pending: OutputColor::default(),
        color_done: false,
        presented_ns: None,
        events: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::OutputColor;
    use super::{Primaries, TransferFunction};

    fn pq() -> OutputColor {
        OutputColor {
            primaries: Primaries::Bt2020 as u32,
            tf: TransferFunction::St2084Pq as u32,
            ..OutputColor::default()
        }
    }

    #[test]
    fn only_bt2020_under_pq_counts_as_hdr() {
        assert!(pq().is_hdr10());
        let wide_sdr = OutputColor {
            tf: TransferFunction::Gamma22 as u32,
            ..pq()
        };
        assert!(!wide_sdr.is_hdr10(), "BT.2020 gamma 2.2 is wide SDR");
        let hlg = OutputColor {
            tf: TransferFunction::Hlg as u32,
            ..pq()
        };
        assert!(!hlg.is_hdr10(), "the stream carries PQ only");
        assert!(!OutputColor::default().is_hdr10());
    }

    #[test]
    fn a_description_without_mastering_data_yields_the_generic_hdr10_block() {
        assert_eq!(pq().hdr_meta(), pf_frame::hdr::generic_hdr10());
    }

    /// Only the fields the compositor sent replace the fallback's.
    #[test]
    fn a_partial_description_keeps_the_fallback_for_the_rest() {
        let c = OutputColor {
            max_cll: Some(600),
            ..pq()
        };
        let want = pf_frame::HdrMeta {
            max_cll: 600,
            ..pf_frame::hdr::generic_hdr10()
        };
        assert_eq!(c.hdr_meta(), want);
    }

    #[test]
    fn mastering_data_converts_to_st2086_units() {
        let c = OutputColor {
            // A DCI-P3 panel: x/y × 1e6 → 1/50000 is ÷20.
            target_primaries: Some([
                (680_000, 320_000),
                (265_000, 690_000),
                (150_000, 60_000),
                (312_700, 329_000),
            ]),
            // 0.0001 cd/m² min, 800 cd/m² max.
            target_luminance: Some((1, 800)),
            max_cll: Some(700),
            max_fall: Some(70_000),
            ..pq()
        };
        let m = c.hdr_meta();
        assert_eq!(
            m.display_primaries,
            [[13250, 34500], [7500, 3000], [34000, 16000]]
        );
        assert_eq!(m.max_display_mastering_luminance, 8_000_000);
        assert_eq!(m.min_display_mastering_luminance, 1);
        assert_eq!(m.max_cll, 700);
        assert_eq!(
            m.max_fall,
            u16::MAX,
            "an out-of-range FALL saturates, never wraps"
        );
    }
}
