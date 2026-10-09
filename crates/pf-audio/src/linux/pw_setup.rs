//! PipeWire bootstrap shared by every loop thread and [`OneShot`](super::pw_oneshot::OneShot):
//! the daemon connection and the F32LE `EnumFormat` pod.

use anyhow::{Context, Result};
use pipewire as pw;

/// Init the library (idempotent), then a main loop and its connection to this session's
/// daemon. `label` prefixes each error. The core holds its context, so the pair keeps the connection alive;
/// guards and listeners borrow them and stay in the caller's frame.
pub(super) fn pw_connect(label: &str) -> Result<(pw::main_loop::MainLoopRc, pw::core::CoreRc)> {
    pw::init();
    let mainloop =
        pw::main_loop::MainLoopRc::new(None).with_context(|| format!("{label} MainLoop"))?;
    let context =
        pw::context::ContextRc::new(&mainloop, None).with_context(|| format!("{label} Context"))?;
    let core = context
        .connect_rc(None)
        .with_context(|| format!("{label} connect (is PipeWire running in this session?)"))?;
    Ok((mainloop, core))
}

/// The `EnumFormat` pod for interleaved F32LE at `rate_hz`, `channels` wide, laid out as
/// `positions` (SPA channel ids, unused slots zero).
pub(super) fn f32_format_pod(rate_hz: u32, channels: u32, positions: [u32; 64]) -> Result<Vec<u8>> {
    use pw::spa;
    use spa::param::audio::{AudioFormat, AudioInfoRaw};
    let mut info = AudioInfoRaw::new();
    info.set_format(AudioFormat::F32LE);
    info.set_rate(rate_hz);
    info.set_channels(channels);
    info.set_position(positions);
    let obj = spa::pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: spa::param::ParamType::EnumFormat.as_raw(),
        properties: info.into(),
    };
    Ok(spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(obj),
    )
    .context("serialize F32LE format pod")?
    .0
    .into_inner())
}
