//! The GameStream RTSP text layer on bytes a LAN peer sends before any pairing check.
//!
//! punktfunk-host is a binary crate, so nothing links it: the std-and-anyhow half of its RTSP
//! module is included by path. A finding is a panic in a pre-auth handler.
#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../../crates/host/punktfunk-host/src/gamestream/rtsp_wire.rs"]
#[allow(dead_code)]
mod rtsp_wire;

fuzz_target!(|data: &[u8]| {
    let mut buf = data.to_vec();
    // The host answers one request per connection; a peer may still send several.
    while let Ok(Some(req)) = rtsp_wire::take_message(&mut buf) {
        let _ = rtsp_wire::stream_type(&req.uri);
        let _ = rtsp_wire::audio_passthrough(&rtsp_wire::parse_announce(&req.body));
    }
});
