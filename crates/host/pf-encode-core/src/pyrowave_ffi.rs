//! The pyrowave-sys calls both PyroWave encoders make the same way: the status check and the
//! packetize step that turns an encoded frame into codec packets for [`crate::pyrowave_wire`].

use anyhow::{bail, Result};
use pyrowave_sys as pw;

pub fn pw_check(r: pw::pyrowave_result, what: &str) -> Result<()> {
    if r == pw::pyrowave_result_PYROWAVE_SUCCESS {
        Ok(())
    } else {
        bail!("pyrowave {what} failed: result {r}")
    }
}

/// Packetize the frame `enc` last encoded into `bitstream` (resized to `cap`), at the boundary
/// `wire_chunk` implies, and stamp the colour bits on the first packet. `(offset, size)` per
/// packet. Dense mode (`None`) is exactly one packet.
///
/// # Safety
/// `enc` is a live encoder whose last encode has completed.
pub unsafe fn packetize(
    enc: pw::pyrowave_encoder,
    bitstream: &mut Vec<u8>,
    cap: usize,
    wire_chunk: Option<usize>,
    pq: bool,
) -> Result<Vec<(usize, usize)>> {
    bitstream.resize(cap, 0);
    // Chunked mode reserves the 4-byte window prefix from the packetize boundary.
    let boundary = crate::pyrowave_wire::packet_boundary(wire_chunk, cap);
    let mut n: usize = 0;
    // SAFETY: the caller's contract; `n` outlives the call.
    let r = unsafe { pw::pyrowave_encoder_compute_num_packets(enc, boundary, &mut n) };
    pw_check(r, "compute_num_packets")?;
    if n == 0 || (wire_chunk.is_none() && n != 1) {
        bail!("pyrowave: unexpected packet count {n} at boundary {boundary}");
    }
    let mut packets = vec![pw::pyrowave_packet { offset: 0, size: 0 }; n];
    let mut out_n: usize = 0;
    // SAFETY: the caller's contract; `packets` holds the `n` entries the encoder asked for and
    // `bitstream` the `cap` bytes it is told about.
    let r = unsafe {
        pw::pyrowave_encoder_packetize(
            enc,
            packets.as_mut_ptr(),
            boundary,
            &mut out_n,
            bitstream.as_mut_ptr() as *mut std::ffi::c_void,
            cap,
        )
    };
    pw_check(r, "packetize")?;
    packets.truncate(out_n.max(1));
    // Pyrowave's C API signals FULL range and centred siting; both CSCs emit limited-range,
    // left-sited codes (BT.2020/PQ when `pq`). Stamp them so VUI-honouring clients keep blacks.
    if let Some(p) = packets.first() {
        crate::pyrowave_wire::stamp_color_bits(bitstream, p.offset, pq);
    }
    Ok(packets.iter().map(|p| (p.offset, p.size)).collect())
}

/// Upstream's CPU decoder as a test oracle for the encoder backends' GPU tests.
#[cfg(any(test, feature = "test-support"))]
pub mod oracle {
    use pyrowave_sys as pw;

    /// Decode `aus` in order through one decoder (its `last_seq` carries across frames) and
    /// return each frame's Y, Cb and Cr planes.
    ///
    /// # Safety
    /// Needs a Vulkan compute device; every `au` is a whole dense AU (or an unwindowed one)
    /// for a `w`×`h` frame at `chroma444`.
    pub unsafe fn decode_planes(
        w: u32,
        h: u32,
        aus: &[&[u8]],
        chroma444: bool,
    ) -> Vec<(Vec<u8>, Vec<u8>, Vec<u8>)> {
        // SAFETY: the caller's contract; every handle below is created and destroyed here, and
        // every buffer the decoder writes is a local sized from the same `w`/`h`.
        unsafe {
            let mut dev: pw::pyrowave_device = std::ptr::null_mut();
            assert_eq!(
                pw::pyrowave_create_default_device(&mut dev),
                pw::pyrowave_result_PYROWAVE_SUCCESS
            );
            let (chroma, format) = if chroma444 {
                (
                    pw::pyrowave_chroma_subsampling_PYROWAVE_CHROMA_SUBSAMPLING_444,
                    pw::pyrowave_cpu_buffer_format_PYROWAVE_CPU_BUFFER_FORMAT_YUV444P,
                )
            } else {
                (
                    pw::pyrowave_chroma_subsampling_PYROWAVE_CHROMA_SUBSAMPLING_420,
                    pw::pyrowave_cpu_buffer_format_PYROWAVE_CPU_BUFFER_FORMAT_YUV420P,
                )
            };
            let dinfo = pw::pyrowave_decoder_create_info {
                device: dev,
                width: w as i32,
                height: h as i32,
                chroma,
                fragment_path: false,
            };
            let mut dec: pw::pyrowave_decoder = std::ptr::null_mut();
            assert_eq!(
                pw::pyrowave_decoder_create(&dinfo, &mut dec),
                pw::pyrowave_result_PYROWAVE_SUCCESS
            );
            let (cw, ch) = if chroma444 { (w, h) } else { (w / 2, h / 2) };
            let mut out = Vec::with_capacity(aus.len());
            for (i, au) in aus.iter().enumerate() {
                assert_eq!(
                    pw::pyrowave_decoder_push_packet(dec, au.as_ptr() as *const _, au.len()),
                    pw::pyrowave_result_PYROWAVE_SUCCESS,
                    "frame {i} was rejected by the decoder"
                );
                assert!(
                    pw::pyrowave_decoder_decode_is_ready(dec, false),
                    "frame {i} never became decodable"
                );
                let mut y = vec![0u8; (w * h) as usize];
                let mut cb = vec![0u8; (cw * ch) as usize];
                let mut cr = vec![0u8; (cw * ch) as usize];
                let mut buf: pw::pyrowave_cpu_buffer = std::mem::zeroed();
                buf.format = format;
                buf.width = w as i32;
                buf.height = h as i32;
                buf.data = [
                    y.as_mut_ptr() as *mut _,
                    cb.as_mut_ptr() as *mut _,
                    cr.as_mut_ptr() as *mut _,
                ];
                buf.row_stride_in_bytes = [w as usize, cw as usize, cw as usize];
                buf.plane_size_in_bytes = [y.len(), cb.len(), cr.len()];
                assert_eq!(
                    pw::pyrowave_decoder_decode_cpu_buffer_synchronous(dec, &buf),
                    pw::pyrowave_result_PYROWAVE_SUCCESS,
                    "frame {i} failed to decode"
                );
                out.push((y, cb, cr));
            }
            pw::pyrowave_decoder_destroy(dec);
            pw::pyrowave_device_destroy(dev);
            out
        }
    }

    /// Mean of each plane of one decoded frame.
    pub fn plane_means(planes: &(Vec<u8>, Vec<u8>, Vec<u8>)) -> (f64, f64, f64) {
        let mean = |v: &[u8]| v.iter().map(|&x| x as f64).sum::<f64>() / v.len() as f64;
        (mean(&planes.0), mean(&planes.1), mean(&planes.2))
    }
}
