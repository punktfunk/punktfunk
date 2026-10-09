//! The stateless flow against a real kernel: `visl`, the virtual stateless
//! decoder, takes the same requests hardware does and validates the controls,
//! then paints a test pattern instead of decoding.
//!
//! `sudo modprobe visl`, then
//! `PF_V4L2_VISL=/dev/videoN cargo test -p pf-v4l2 -- --ignored`.

#![cfg(target_os = "linux")]

use std::path::Path;

use pf_bitstream::h265::H265Planner;
use pf_bitstream::testing::split_h265_aus;
use pf_bitstream::testing::H265_25FPS;
use pf_v4l2::RequestNode;
use pf_v4l2dec::stateful::Queue;
use pf_v4l2dec::stateless::HevcDecoder;
use pf_v4l2dec::uapi::V4L2_PIX_FMT_NV12;
use pf_v4l2dec::uapi_stateless::V4L2_PIX_FMT_HEVC_SLICE;

#[test]
#[ignore = "needs the kernel's visl decoder: PF_V4L2_VISL=/dev/videoN"]
fn the_vendored_hevc_stream_decodes_on_visl() {
    let path = std::env::var("PF_V4L2_VISL").expect("PF_V4L2_VISL names the visl node");
    let node = RequestNode::open(Path::new(&path)).expect("open the node and its request");
    let coded = node.formats(Queue::Output).expect("enumerate");
    assert!(coded.contains(&V4L2_PIX_FMT_HEVC_SLICE), "{coded:x?}");

    let aus = split_h265_aus(H265_25FPS);
    let mut planner = H265Planner::new();
    let mut node = Some(node);
    let mut decoder: Option<HevcDecoder<RequestNode, usize>> = None;
    let mut shown = 0usize;
    for (n, au) in aus.iter().enumerate() {
        let plan = planner.plan_au(au).expect("the vector plans");
        let d = decoder.get_or_insert_with(|| {
            let node = node.take().expect("opened once");
            HevcDecoder::open(node, &plan, au, &[V4L2_PIX_FMT_NV12]).expect("start the decoder")
        });
        let format = d.format();
        for picture in d
            .decode(&plan, au, n)
            .unwrap_or_else(|e| panic!("access unit {n}: {e}"))
        {
            assert!(!picture.corrupt, "access unit {n}");
            let bytes = d.device().picture(picture.buffer).expect("mapped");
            let luma = (format.stride * format.height) as usize;
            assert!(bytes.len() >= luma * 3 / 2, "{} bytes", bytes.len());
            // The pattern generator wrote something: not an untouched buffer.
            assert!(bytes[..luma].iter().any(|b| *b != bytes[0]));
            d.release(picture.buffer);
            shown += 1;
        }
    }
    let mut d = decoder.expect("the vector has pictures");
    let format = d.format();
    assert_eq!(format.fourcc, V4L2_PIX_FMT_NV12);
    assert!((format.width, format.height) >= (320, 240), "{format:?}");
    assert!(shown > 240, "{shown} of 250 pictures shown");
    d.device().export(0).expect("export a picture buffer");
}
