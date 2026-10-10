//! Client bitstream layer for native decode (`design/client-native-decode.md`).
//!
//! Per AU: headers, POC, DPB, reference lists (MMCO/LTR), recovery-point SEI —
//! derived once, consumed by every stateless backend (Vulkan `StdVideo*`, DXVA,
//! libva). Parsing primitives come from vendored `vendor/cros-codecs` (see its
//! PROVENANCE.md). This crate owns the per-AU orchestration upstream keeps in
//! Linux-only `decoder::stateless`, plus SEI payload parsing (upstream classifies
//! SEI NALUs but never reads them).
//!
//! Scope: punktfunk hosts — zero-reorder, no B-frames, progressive, controlled
//! parameter sets. Implement to spec where cheap; reject-with-log outside that
//! envelope. CPU-only: no GPU API, OS handle, or network.
#![forbid(unsafe_code)]

pub mod av1;
pub mod clean;
pub mod h264;
pub mod h265;
mod report;
pub mod sei;
pub mod slots;
#[cfg(any(test, feature = "test-vectors"))]
pub mod testing;

#[cfg(test)]
mod plan_error_tests {
    use crate::{h264, h265};

    /// The idle rule the Vulkan, VAAPI and D3D11VA rungs all read.
    #[test]
    fn only_a_missing_idr_or_parameter_set_is_idle() {
        assert!(h264::PlanError::AwaitingIdr.awaits_idr());
        assert!(h264::PlanError::NoActiveParamSet { pps_id: 0 }.awaits_idr());
        assert!(!h264::PlanError::Parse(String::new()).awaits_idr());
        assert!(!h264::PlanError::OutsideEnvelope("").awaits_idr());
        assert!(h265::PlanError::AwaitingIdr.awaits_idr());
        assert!(h265::PlanError::NoActiveParamSet { pps_id: 0 }.awaits_idr());
        assert!(!h265::PlanError::RaslSkipped { poc: 0 }.awaits_idr());
        assert!(!h265::PlanError::Parse(String::new()).awaits_idr());
        assert!(!h265::PlanError::OutsideEnvelope("").awaits_idr());
    }
}

#[cfg(test)]
mod integrity_tests {
    use crate::{av1, h264, h265};

    #[test]
    fn damage_is_a_lost_reference_or_a_short_au_and_nothing_else() {
        for w in [
            h264::PlanWarning::FrameNumGap {
                expected: 4,
                got: 7,
            },
            h264::PlanWarning::MissingReference {
                context: "list0",
                detail: "poc 12".into(),
            },
            h264::PlanWarning::TruncatedAu { offset: 900 },
        ] {
            assert!(w.is_integrity(), "{w:?} is damage");
        }
        assert!(
            !h264::PlanWarning::Mmco5Rebase.is_integrity(),
            "an MMCO 5 was planned in FULL — dropping its frame would hitch a \
             correct stream"
        );

        for w in [
            h265::PlanWarning::MissingReference {
                context: "StCurrBefore",
                detail: "poc 12".into(),
            },
            h265::PlanWarning::TruncatedAu { offset: 900 },
        ] {
            assert!(w.is_integrity(), "{w:?} is damage");
        }
        assert!(
            !h265::PlanWarning::NonZeroReorder {
                max_num_reorder_pics: 1
            }
            .is_integrity(),
            "SPS activation is not damage — it fires on the opening IDR and on \
             every ABR renegotiation's IDR"
        );
    }

    /// Guards reclassification of an AV1 warning as clean. A new variant is
    /// caught by the exhaustive match, not this hand list.
    /// `MissingShowExisting` is damage: the screen keeps the previous picture.
    #[test]
    fn every_av1_warning_is_damage_because_av1_has_no_envelope_signal() {
        for w in [
            av1::PlanWarning::MissingReference {
                slot: 3,
                ref_index: 1,
            },
            av1::PlanWarning::MissingShowExisting { slot: 5 },
            av1::PlanWarning::TruncatedAu { offset: 900 },
        ] {
            assert!(w.is_integrity(), "{w:?} is damage");
        }
    }
}

// Golden counts from the vendored snapshot's own vectors. A cros-codecs re-sync that
// shifts parser behaviour must trip here, not in a decode session.
#[cfg(test)]
mod vendor_smoke {
    use std::io::Cursor;

    use cros_codecs::bitstream_utils::IvfIterator;
    use cros_codecs::codec::av1::parser::ObuAction;
    use cros_codecs::codec::av1::parser::ParsedObu;
    use cros_codecs::codec::h264::parser::Nalu as H264Nalu;
    use cros_codecs::codec::h264::parser::Parser as H264Parser;
    use cros_codecs::codec::h265::parser::Nalu as H265Nalu;
    use cros_codecs::codec::h265::parser::Parser as H265Parser;

    use crate::testing::AV1_25FPS;
    use crate::testing::H264_25FPS;
    use crate::testing::H265_25FPS;
    use crate::testing::VP9_25FPS;

    #[test]
    fn h264_parses_the_vendored_vector_to_its_goldens() {
        let mut cursor = Cursor::new(H264_25FPS);
        let mut parser = H264Parser::default();
        let (mut nalus, mut sps, mut slices) = (0u32, 0u32, 0u32);
        let mut coded = (0u32, 0u32);
        while let Ok(nalu) = H264Nalu::next(&mut cursor) {
            nalus += 1;
            if let Ok(s) = parser.parse_sps(&nalu) {
                sps += 1;
                coded = (
                    (s.pic_width_in_mbs_minus1 as u32 + 1) * 16,
                    (s.pic_height_in_map_units_minus1 as u32 + 1) * 16,
                );
                continue;
            }
            if parser.parse_pps(&nalu).is_ok() {
                continue;
            }
            if parser.parse_slice_header(nalu).is_ok() {
                slices += 1;
            }
        }
        // 759 is upstream's golden (chromium h264_parser_unittest lineage).
        assert_eq!(nalus, 759);
        assert_eq!(sps, 4);
        assert_eq!(slices, 500);
        assert_eq!(coded, (320, 240));
    }

    #[test]
    fn h265_parses_the_vendored_vector() {
        let mut cursor = Cursor::new(H265_25FPS);
        let mut parser = H265Parser::default();
        let (mut nalus, mut sps, mut slices) = (0u32, 0u32, 0u32);
        while let Ok(nalu) = H265Nalu::next(&mut cursor) {
            nalus += 1;
            if parser.parse_sps(&nalu).is_ok() {
                sps += 1;
                continue;
            }
            if parser.parse_pps(&nalu).is_ok() {
                continue;
            }
            if parser.parse_slice_header(nalu).is_ok() {
                slices += 1;
            }
        }
        assert_eq!(nalus, 254);
        assert_eq!(sps, 1);
        assert_eq!(slices, 250);
    }

    #[test]
    fn av1_walks_obus_and_maintains_ref_slots_across_the_stream() {
        let mut parser = cros_codecs::codec::av1::parser::Parser::default();
        let (mut obus, mut frames) = (0u32, 0u32);
        for packet in IvfIterator::new(AV1_25FPS) {
            let mut consumed = 0;
            while let Ok(action) = parser.read_obu(&packet[consumed..]) {
                let obu = match action {
                    ObuAction::Process(obu) => obu,
                    ObuAction::Drop(n) => {
                        consumed += n as usize;
                        continue;
                    }
                };
                consumed += obu.bytes_used;
                obus += 1;
                // Without `ref_frame_update` the next inter frame fails with "Reference is invalid".
                match parser.parse_obu(obu).expect("parse_obu") {
                    ParsedObu::FrameHeader(fh) => {
                        frames += 1;
                        parser.ref_frame_update(&fh).expect("ref slot update");
                    }
                    ParsedObu::Frame(f) => {
                        frames += 1;
                        parser.ref_frame_update(&f.header).expect("ref slot update");
                    }
                    _ => {}
                }
            }
        }
        // 525 is upstream's golden (cross-checked against GStreamer's OBU walk).
        assert_eq!(obus, 525);
        assert_eq!(frames, 274);
    }

    #[test]
    fn vp9_splits_superframes_and_parses_headers() {
        let mut parser = cros_codecs::codec::vp9::parser::Parser::default();
        let (mut chunks, mut frames) = (0u32, 0u32);
        for packet in IvfIterator::new(VP9_25FPS) {
            chunks += 1;
            frames += parser
                .parse_chunk(packet.as_ref())
                .expect("vp9 chunk")
                .len() as u32;
        }
        assert_eq!(chunks, 250);
        // frames > chunks: superframe splitting engaged.
        assert_eq!(frames, 269);
    }
}
