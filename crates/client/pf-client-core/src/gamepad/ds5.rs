//! DualSense effects packets the worker hands to SDL's raw-effect call.

/// DualSense effects packet (SDL `DS5EffectsState_t`, 47 bytes). Offsets are the USB
/// output report **minus one**: SDL's payload has no leading report id. Deliberate
/// second copy — `pf-inject` owns the layout and this crate cannot import it.
/// [`ds5_offsets_track_the_usb_report`](ds5_feedback_tests) pins the `−1`.
pub(super) struct Ds5Feedback;

impl Ds5Feedback {
    const REPORT_ID_LEN: usize = 1;
    /// Audio-control region (`ucHeadphoneVolume`…`ucAudioMuteBits`): USB report byte 5.
    const AUDIO: usize = 5 - Self::REPORT_ID_LEN;
    const RIGHT_TRIGGER: usize = 11 - Self::REPORT_ID_LEN;
    const LEFT_TRIGGER: usize = 22 - Self::REPORT_ID_LEN;
    const PAD_LIGHTS: usize = 44 - Self::REPORT_ID_LEN;
    /// `ucMicLightMode`: USB report byte 9.
    const MIC_LED: usize = 9 - Self::REPORT_ID_LEN;
    /// Audio-control-2 (speaker preamp gain in bits 0..2): USB report byte 38.
    const AUDIO2: usize = 38 - Self::REPORT_ID_LEN;
    const LED_RGB: usize = 45 - Self::REPORT_ID_LEN;
    /// Mode byte plus 10 parameters: the wire's trigger-effect clamp.
    const TRIGGER_LEN: usize = punktfunk_core::quic::TRIGGER_EFFECT_MAX;

    pub(super) fn trigger_packet(which: u8, effect: &[u8]) -> [u8; 47] {
        let mut p = [0u8; 47];
        let (flag, off) = if which == 1 {
            (0x04, Self::RIGHT_TRIGGER)
        } else {
            (0x08, Self::LEFT_TRIGGER)
        };
        p[0] = flag;
        let n = effect.len().min(Self::TRIGGER_LEN);
        p[off..off + n].copy_from_slice(&effect[..n]);
        p
    }

    pub(super) fn lightbar_packet(r: u8, g: u8, b: u8) -> [u8; 47] {
        let mut p = [0u8; 47];
        p[1] = 0x04; // valid_flag1 lightbar
        p[Self::LED_RGB] = r;
        p[Self::LED_RGB + 1] = g;
        p[Self::LED_RGB + 2] = b;
        p
    }

    pub(super) fn player_packet(bits: u8) -> [u8; 47] {
        let mut p = [0u8; 47];
        p[1] = 0x10; // valid_flag1 player LEDs
        p[Self::PAD_LIGHTS] = bits & 0x1F;
        p
    }

    pub(super) fn mic_led_packet(mode: u8) -> [u8; 47] {
        let mut p = [0u8; 47];
        p[1] = 0x01; // valid_flag1 mic-mute LED
        p[Self::MIC_LED] = mode;
        p
    }

    /// All-zero packet: `ucEnableBits1` bits 0/1 stay clear. SDL's rumble path sets both
    /// ("enable rumble emulation" + "disable audio haptics"), which mutes the 0xD1 coils.
    pub(super) fn audio_haptics_packet() -> [u8; 47] {
        [0u8; 47]
    }

    /// Point channel 1 (shared headphone-R / mono speaker) at the speaker. Power-on is
    /// the jack, so the speaker pair is silent until the output path selects it: `0x20`
    /// (L-L R) keeps a headphone copy, `0x30` (X-X R) mutes the jack.
    /// Volume `0x64` and preamp `+6 dB` (`2`) are Linux `hid-playstation`'s speaker
    /// route; the pad's speaker volume range is `0x3D..=0x64`.
    /// Bits 0/1 stay clear (rumble-emulation / disable-audio-haptics mute the coils).
    /// The select persists across USB-audio restart; a later
    /// [`HidOutput::AudioCtl`](punktfunk_core::quic::HidOutput::AudioCtl) still overrides. `PUNKTFUNK_PAD_SPEAKER_PATH` / `_VOLUME` override per run.
    pub(super) fn speaker_enable_packet(volume: u8, path: u8) -> [u8; 47] {
        let mut p = [0u8; 47];
        // bit5 = ucSpeakerVolume valid, bit7 = audio-control byte valid.
        p[0] = 0x20 | 0x80;
        p[1] = 0x80; // audio-control-2 valid
        p[Self::AUDIO + 1] = volume;
        p[Self::AUDIO + 3] = path;
        p[Self::AUDIO2] = 0x02;
        p
    }

    /// Fold [`HidOutput::AudioCtl`](punktfunk_core::quic::HidOutput::AudioCtl): `raw` is
    /// report `0x02` bytes 5..=10 → offsets 4..=9.
    /// `flags` bits 1..4 become `p[0]` bits 4..7. Bit 0 is not replayed — bits 0/1 stay
    /// clear so audio haptics stay live ([`audio_haptics_packet`]).
    pub(super) fn audio_ctl_packet(flags: u8, raw: &[u8; 6]) -> [u8; 47] {
        let mut p = [0u8; 47];
        p[0] = (flags & 0x1E) << 3;
        p[Self::AUDIO..Self::AUDIO + 6].copy_from_slice(raw);
        p
    }
}

#[cfg(test)]
mod ds5_feedback_tests {
    use super::*;

    /// SDL payload offsets are USB report offsets minus the leading report id.
    #[test]
    fn ds5_offsets_track_the_usb_report() {
        for (usb, payload) in [
            (11usize, Ds5Feedback::RIGHT_TRIGGER),
            (22, Ds5Feedback::LEFT_TRIGGER),
            (44, Ds5Feedback::PAD_LIGHTS),
            (45, Ds5Feedback::LED_RGB),
            (9, Ds5Feedback::MIC_LED),
            (38, Ds5Feedback::AUDIO2),
        ] {
            assert_eq!(payload, usb - 1, "payload offset for USB byte {usb}");
        }
        assert_eq!(Ds5Feedback::TRIGGER_LEN, 11);
    }

    #[test]
    fn lightbar_sets_only_its_enable_bit_and_its_three_bytes() {
        let p = Ds5Feedback::lightbar_packet(0x11, 0x22, 0x33);
        assert_eq!(p.len(), 47);
        assert_eq!(p[1], 0x04, "valid_flag1 lightbar bit");
        assert_eq!(p[0], 0, "must not claim any valid_flag0 field");
        assert_eq!(
            (
                p[Ds5Feedback::LED_RGB],
                p[Ds5Feedback::LED_RGB + 1],
                p[Ds5Feedback::LED_RGB + 2]
            ),
            (0x11, 0x22, 0x33)
        );
        let touched = [
            1,
            Ds5Feedback::LED_RGB,
            Ds5Feedback::LED_RGB + 1,
            Ds5Feedback::LED_RGB + 2,
        ];
        assert!(p
            .iter()
            .enumerate()
            .all(|(i, &b)| touched.contains(&i) || b == 0));
    }

    #[test]
    fn player_leds_are_masked_to_five_bits() {
        let p = Ds5Feedback::player_packet(0xFF);
        assert_eq!(p[1], 0x10, "valid_flag1 player-indicator bit");
        assert_eq!(
            p[Ds5Feedback::PAD_LIGHTS],
            0x1F,
            "high bits are not ours to set"
        );
        let p = Ds5Feedback::player_packet(0b0000_0101);
        assert_eq!(p[Ds5Feedback::PAD_LIGHTS], 0b0000_0101);
    }

    /// SDL's `ucEnableBits2` 0x01 enables `ucMicLightMode`; nothing else is claimed.
    #[test]
    fn mic_led_sets_only_its_enable_bit_and_mode() {
        let p = Ds5Feedback::mic_led_packet(2);
        assert_eq!(p[1], 0x01, "valid_flag1 mic-mute LED bit");
        assert_eq!(p[0], 0, "must not claim any valid_flag0 field");
        assert_eq!(p[Ds5Feedback::MIC_LED], 2);
        assert_eq!(p.iter().filter(|&&b| b != 0).count(), 2);
    }

    /// which 1 = R2, which 0 = L2; the RIGHT block sits first in the report.
    #[test]
    fn trigger_which_selects_the_right_flag_and_offset() {
        let eff: Vec<u8> = (1..=11).collect();

        let r = Ds5Feedback::trigger_packet(1, &eff);
        assert_eq!(r[0], 0x04, "valid_flag0 R2 bit");
        assert_eq!(
            &r[Ds5Feedback::RIGHT_TRIGGER..Ds5Feedback::RIGHT_TRIGGER + 11],
            &eff[..]
        );
        assert_eq!(
            r[Ds5Feedback::LEFT_TRIGGER],
            0,
            "the other trigger is untouched"
        );

        let l = Ds5Feedback::trigger_packet(0, &eff);
        assert_eq!(l[0], 0x08, "valid_flag0 L2 bit");
        assert_eq!(
            &l[Ds5Feedback::LEFT_TRIGGER..Ds5Feedback::LEFT_TRIGGER + 11],
            &eff[..]
        );
        assert_eq!(l[Ds5Feedback::RIGHT_TRIGGER], 0);
    }

    #[test]
    fn an_oversized_effect_is_clamped_rather_than_overflowing_into_the_next_field() {
        let long = vec![0xAAu8; 40];
        let p = Ds5Feedback::trigger_packet(1, &long);
        assert_eq!(p.len(), 47);
        assert_eq!(p[Ds5Feedback::RIGHT_TRIGGER + 10], 0xAA);
        assert_eq!(p[Ds5Feedback::RIGHT_TRIGGER + 11], 0);
        assert_eq!(p[Ds5Feedback::LEFT_TRIGGER], 0);
    }

    #[test]
    fn a_short_effect_leaves_the_rest_of_the_block_zeroed() {
        let p = Ds5Feedback::trigger_packet(0, &[0x02, 0x99]);
        assert_eq!(p[Ds5Feedback::LEFT_TRIGGER], 0x02);
        assert_eq!(p[Ds5Feedback::LEFT_TRIGGER + 1], 0x99);
        assert!(
            p[Ds5Feedback::LEFT_TRIGGER + 2..Ds5Feedback::LEFT_TRIGGER + 11]
                .iter()
                .all(|&b| b == 0)
        );
    }

    /// Empty effect is mode 0x00 = release. The enable bit must still be set, or the pad
    /// keeps the latched effect.
    #[test]
    fn an_empty_effect_is_a_release_not_a_no_op() {
        let p = Ds5Feedback::trigger_packet(1, &[]);
        assert_eq!(p[0], 0x04);
        assert!(
            p[Ds5Feedback::RIGHT_TRIGGER..Ds5Feedback::RIGHT_TRIGGER + 11]
                .iter()
                .all(|&b| b == 0)
        );
    }
}

#[cfg(test)]
mod reset_packet_tests {
    use super::*;

    /// Wrong enable flag or a non-zero mode byte leaves the effect latched.
    #[test]
    fn reset_packets_release_the_triggers_and_darken_the_lights() {
        let l = Ds5Feedback::trigger_packet(0, &[0u8; 11]);
        assert_eq!(l[0], 0x08, "left-trigger enable bit");
        assert!(
            l[Ds5Feedback::LEFT_TRIGGER..Ds5Feedback::LEFT_TRIGGER + 11]
                .iter()
                .all(|&b| b == 0),
            "an all-zero block is mode 0x00 = no effect"
        );
        let r = Ds5Feedback::trigger_packet(1, &[0u8; 11]);
        assert_eq!(r[0], 0x04, "right-trigger enable bit");
        assert!(
            r[Ds5Feedback::RIGHT_TRIGGER..Ds5Feedback::RIGHT_TRIGGER + 11]
                .iter()
                .all(|&b| b == 0)
        );

        let bar = Ds5Feedback::lightbar_packet(0, 0, 0);
        assert_eq!(bar[1], 0x04, "lightbar enable bit");
        assert_eq!(
            &bar[Ds5Feedback::LED_RGB..Ds5Feedback::LED_RGB + 3],
            &[0, 0, 0]
        );

        let pl = Ds5Feedback::player_packet(0);
        assert_eq!(pl[1], 0x10, "player-LED enable bit");
        assert_eq!(pl[Ds5Feedback::PAD_LIGHTS], 0);
    }
}
