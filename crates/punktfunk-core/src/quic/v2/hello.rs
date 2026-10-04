//! The v2 handshake: `ClientHello` → `ServerHello` → `Ready`, carrying what `punktfunk/1` spread
//! over `Hello`, `Start`'s extension block and `Welcome`.
//!
//! Both ends keep `punktfunk/1`'s structs as the model: a [`ClientHello`] decodes to a [`Hello`]
//! plus the `Start` extension entries, a [`ServerHello`] to a [`Welcome`]. Decoding applies the
//! same folding `punktfunk/1`'s decoders do (unknown codec to HEVC, unsupported rate to 48 kHz,
//! bad name to none), so host and client logic sees what it always saw;
//! `tests::v1_and_v2_decode_alike` holds the two wires to that.
//!
//! What only `punktfunk/1` carries is absent here: the wire version (ALPN picks the wire), the
//! data port (media rides the QUIC path) and the session key (both ends derive it).

use super::features::FeatureSet;
use super::field::*;
use super::msg::{FieldValue, V2Message};
use super::registry as reg;
use crate::config::{CompositorPref, FecConfig, FecScheme, GamepadPref, Mode};
use crate::crypto::MediaSuite;
use crate::error::Result;
use crate::quic::*;

/// `Start` extension tags and the `ClientHello` tags their values ride under, unchanged.
const START_EXT: [(u16, u64); 4] = [
    (EXT_TAG_CLIENT, 19),
    (EXT_TAG_ABR, 20),
    (EXT_TAG_PRESET, 21),
    (EXT_TAG_DELIVERY, 22),
];

/// Wire id of a media suite in `ClientHello` and `ServerHello`.
fn suite_id(s: MediaSuite) -> u8 {
    match s {
        MediaSuite::Aes128Gcm => 0,
        MediaSuite::ChaCha20Poly1305 => 1,
    }
}

fn suite_of(id: u8) -> Option<MediaSuite> {
    match id {
        0 => Some(MediaSuite::Aes128Gcm),
        1 => Some(MediaSuite::ChaCha20Poly1305),
        _ => None,
    }
}

/// `client → host`, first frame of the control stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientHello {
    pub hello: Hello,
    /// The entries a client adds ([`EXT_TAG_CLIENT`] and after), by tag.
    pub start_ext: Vec<(u16, Vec<u8>)>,
    /// The session this client held, to take back after a drop.
    pub resume: Option<[u8; 16]>,
    /// Media AEADs the client takes, most wanted first. Empty on a carrier that encrypts.
    pub suites: Vec<MediaSuite>,
    /// Bits from 32 up ([`FeatureSet::native`]); `hello`'s capability bytes fill 0–31.
    pub features: FeatureSet,
}

impl ClientHello {
    /// The extension entries as the host reads `Start`'s: `(tag, value)`.
    pub fn ext_entries(&self) -> Vec<(u16, &[u8])> {
        self.start_ext
            .iter()
            .map(|(t, v)| (*t, v.as_slice()))
            .collect()
    }
}

impl V2Message for ClientHello {
    const TYPE: u64 = reg::MSG_CLIENT_HELLO;

    fn fields(&self) -> Fields {
        let h = &self.hello;
        let features = FeatureSet::client(h.video_caps, h.client_caps)
            .union(self.features.native())
            .encode();
        let suites: Vec<u8> = self.suites.iter().map(|&s| suite_id(s)).collect();
        let mut f = Fields::new()
            .when(self.resume.is_some(), |f| {
                f.bytes(1, &self.resume.unwrap_or_default())
            })
            .when(!suites.is_empty(), |f| f.bytes(2, &suites));
        f = h.mode.put(f, 3);
        f = f
            .u8(4, h.compositor.to_u8())
            .u8(5, h.gamepad.to_u8())
            .u32(6, h.bitrate_kbps)
            .when(h.name.is_some(), |f| {
                f.str(
                    7,
                    truncate_to(h.name.as_deref().unwrap_or(""), HELLO_NAME_MAX),
                )
            })
            .when(h.launch.is_some(), |f| {
                f.str(
                    8,
                    truncate_to(h.launch.as_deref().unwrap_or(""), HELLO_LAUNCH_MAX),
                )
            })
            .bytes(9, &features)
            .u8(10, h.audio_channels)
            .u8(11, h.video_codecs)
            .u8(12, h.preferred_codec)
            .when(h.display_hdr.is_some(), |f| {
                let mut b = Vec::with_capacity(super::super::HDR_META_BODY_LEN);
                write_hdr_meta_body(&h.display_hdr.unwrap_or_default(), &mut b);
                f.bytes(13, &b)
            })
            .u16(14, h.max_shard_payload)
            .u32(15, h.audio_rate_hz)
            .u8(16, h.audio_bits)
            .u8(17, h.audio_layout)
            .u8(18, h.video_fit);
        for (v1, v2) in START_EXT {
            if let Some((_, v)) = self.start_ext.iter().find(|(t, _)| *t == v1) {
                f = f.bytes(v2, v);
            }
        }
        f
    }

    fn from_body(body: &[u8]) -> Result<Self> {
        let mut h = Hello {
            mode: Mode::default(),
            compositor: CompositorPref::Auto,
            gamepad: GamepadPref::Auto,
            bitrate_kbps: 0,
            name: None,
            launch: None,
            video_caps: 0,
            audio_channels: 2,
            video_codecs: 0,
            preferred_codec: 0,
            display_hdr: None,
            client_caps: 0,
            max_shard_payload: 0,
            audio_rate_hz: 0,
            audio_bits: 0,
            audio_layout: 0,
            video_fit: 0,
        };
        let (mut resume, mut suites, mut start_ext) = (None, Vec::new(), Vec::new());
        let mut features = FeatureSet::default();
        let mut seen = Vec::new();
        let mut r = FieldReader::new(body);
        while let Some((tag, v)) = r.next_field()? {
            if seen.contains(&tag) {
                return Err(crate::error::PunktfunkError::InvalidArg("repeated field"));
            }
            seen.push(tag);
            match tag {
                1 => resume = Some(<[u8; 16]>::try_from(v).map_err(|_| bad())?),
                2 => suites = v.iter().filter_map(|&id| suite_of(id)).collect(),
                3 => h.mode = Mode::get(v)?,
                4 => h.compositor = CompositorPref::from_u8(u8_of(v)?),
                5 => h.gamepad = GamepadPref::from_u8(u8_of(v)?),
                6 => h.bitrate_kbps = u32_of(v)?,
                // A label never fails the handshake: out of bounds or not UTF-8 is no label.
                7 => h.name = label(v, HELLO_NAME_MAX),
                8 => h.launch = label(v, HELLO_LAUNCH_MAX),
                9 => {
                    let f = FeatureSet::decode(v);
                    (h.video_caps, h.client_caps) = (f.video_caps(), f.client_caps());
                    features = f.native();
                }
                10 => h.audio_channels = u8_of(v)?,
                11 => h.video_codecs = u8_of(v)?,
                12 => h.preferred_codec = u8_of(v)?,
                13 => {
                    h.display_hdr =
                        (v.len() == super::super::HDR_META_BODY_LEN).then(|| read_hdr_meta_body(v))
                }
                14 => h.max_shard_payload = u16_of(v)?,
                15 => h.audio_rate_hz = u32_of(v)?,
                16 => h.audio_bits = u8_of(v)?,
                17 => h.audio_layout = u8_of(v)?,
                18 => h.video_fit = u8_of(v)?,
                _ => {
                    if let Some((v1, _)) = START_EXT.iter().find(|(_, v2)| *v2 == tag) {
                        start_ext.push((*v1, v.to_vec()));
                    }
                }
            }
        }
        // Fold what this build cannot honour onto its default.
        h.audio_channels = crate::audio::normalize_channels(h.audio_channels);
        if h.audio_rate_hz == 0 {
            h.audio_rate_hz = crate::audio::SAMPLE_RATE_HZ;
        }
        if !crate::audio::pcm::depth_is_supported(h.audio_bits) {
            h.audio_bits = crate::audio::pcm::BITS_16;
        }
        Ok(ClientHello {
            hello: h,
            start_ext,
            resume,
            suites,
            features,
        })
    }
}

fn bad() -> crate::error::PunktfunkError {
    crate::error::PunktfunkError::InvalidArg("bad handshake field")
}

fn label(v: &[u8], max: usize) -> Option<String> {
    (!v.is_empty() && v.len() <= max)
        .then(|| std::str::from_utf8(v).ok().map(String::from))
        .flatten()
}

/// `host → client`: the session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ServerHello {
    pub welcome: Welcome,
    /// Names this session for a reconnect ([`ClientHello::resume`]) and binds the media keys.
    pub session_id: [u8; 16],
    /// The host instant, Unix ns, that media capture time 0 stands for.
    pub clock_origin_ns: u64,
    /// The media AEAD; `None` on a carrier that already encrypts.
    pub suite: Option<MediaSuite>,
    /// Native bits in force: the ones both ends set ([`ClientHello::features`]).
    pub features: FeatureSet,
}

impl V2Message for ServerHello {
    const TYPE: u64 = reg::MSG_SERVER_HELLO;

    fn fields(&self) -> Fields {
        let w = &self.welcome;
        let features = FeatureSet::host(w.host_caps, w.host_caps2)
            .union(self.features.native())
            .encode();
        let c = w.color;
        let mut f = Fields::new()
            .bytes(1, &self.session_id)
            .u64(2, self.clock_origin_ns)
            .when(self.suite.is_some(), |f| {
                f.u8(3, self.suite.map_or(0, suite_id))
            })
            .bytes(4, &features);
        f = w.mode.put(f, 5);
        f.u8(6, w.fec.scheme as u8)
            .u8(7, w.fec.fec_percent)
            .u16(8, w.fec.max_data_per_block)
            .u16(9, w.shard_payload)
            .u32(10, w.frames)
            .u8(11, w.compositor.to_u8())
            .u8(12, w.gamepad.to_u8())
            .u32(13, w.bitrate_kbps)
            .u8(14, w.bit_depth)
            .bytes(15, &[c.primaries, c.transfer, c.matrix, c.full_range])
            .u8(16, w.chroma_format)
            .u8(17, w.audio_channels)
            .u8(18, w.codec)
            .u16(19, w.mgmt_port)
            .u32(20, w.grants)
            .u32(21, w.expires_in_secs)
            .u8(22, w.audio_codec)
            .u32(23, w.audio_rate_hz)
            .u8(24, w.audio_bits)
            .u16(25, w.audio_frame_us)
            .u8(26, w.audio_layout)
    }

    fn from_body(body: &[u8]) -> Result<Self> {
        let mut w = Welcome {
            mode: Mode::default(),
            fec: FecConfig {
                scheme: FecScheme::Gf16,
                fec_percent: 0,
                max_data_per_block: 0,
            },
            shard_payload: 0,
            frames: 0,
            compositor: CompositorPref::Auto,
            gamepad: GamepadPref::Auto,
            bitrate_kbps: 0,
            bit_depth: 8,
            color: ColorInfo::SDR_BT709,
            chroma_format: CHROMA_IDC_420,
            audio_channels: 2,
            codec: CODEC_HEVC,
            host_caps: 0,
            mgmt_port: 0,
            grants: GRANT_ALL,
            expires_in_secs: 0,
            audio_codec: AUDIO_CODEC_OPUS,
            audio_rate_hz: crate::audio::SAMPLE_RATE_HZ,
            audio_bits: crate::audio::pcm::BITS_16,
            audio_frame_us: 0,
            host_caps2: 0,
            audio_layout: 0,
        };
        let (mut session_id, mut clock_origin_ns, mut suite) = (None, 0, None);
        let mut features = FeatureSet::default();
        let mut seen = Vec::new();
        let mut r = FieldReader::new(body);
        while let Some((tag, v)) = r.next_field()? {
            if seen.contains(&tag) {
                return Err(crate::error::PunktfunkError::InvalidArg("repeated field"));
            }
            seen.push(tag);
            match tag {
                1 => session_id = Some(<[u8; 16]>::try_from(v).map_err(|_| bad())?),
                2 => clock_origin_ns = u64_of(v)?,
                // An id this build does not know cannot open the media; refuse the session.
                3 => suite = Some(suite_of(u8_of(v)?).ok_or_else(bad)?),
                4 => {
                    let f = FeatureSet::decode(v);
                    (w.host_caps, w.host_caps2) = (f.host_caps(), f.host_caps2());
                    features = f.native();
                }
                5 => w.mode = Mode::get(v)?,
                6 => {
                    w.fec.scheme = if u8_of(v)? == 1 {
                        FecScheme::Gf16
                    } else {
                        FecScheme::Gf8
                    }
                }
                7 => w.fec.fec_percent = u8_of(v)?,
                8 => w.fec.max_data_per_block = u16_of(v)?,
                9 => w.shard_payload = u16_of(v)?,
                10 => w.frames = u32_of(v)?,
                11 => w.compositor = CompositorPref::from_u8(u8_of(v)?),
                12 => w.gamepad = GamepadPref::from_u8(u8_of(v)?),
                13 => w.bitrate_kbps = u32_of(v)?,
                14 => w.bit_depth = u8_of(v)?,
                15 => {
                    let c: [u8; 4] = v.try_into().map_err(|_| bad())?;
                    w.color = ColorInfo {
                        primaries: c[0],
                        transfer: c[1],
                        matrix: c[2],
                        full_range: c[3],
                    };
                }
                16 => w.chroma_format = u8_of(v)?,
                17 => w.audio_channels = u8_of(v)?,
                18 => w.codec = u8_of(v)?,
                19 => w.mgmt_port = u16_of(v)?,
                20 => w.grants = u32_of(v)?,
                21 => w.expires_in_secs = u32_of(v)?,
                22 => w.audio_codec = u8_of(v)?,
                23 => w.audio_rate_hz = u32_of(v)?,
                24 => w.audio_bits = u8_of(v)?,
                25 => w.audio_frame_us = u16_of(v)?,
                26 => w.audio_layout = u8_of(v)?,
                _ => {}
            }
        }
        // Fold what this build cannot honour onto its default.
        if w.chroma_format != CHROMA_IDC_444 {
            w.chroma_format = CHROMA_IDC_420;
        }
        w.audio_channels = crate::audio::normalize_channels(w.audio_channels);
        if !matches!(w.codec, CODEC_H264 | CODEC_AV1 | CODEC_PYROWAVE) {
            w.codec = CODEC_HEVC;
        }
        if !crate::audio::pcm::rate_is_supported(w.audio_rate_hz) {
            w.audio_rate_hz = crate::audio::SAMPLE_RATE_HZ;
        }
        if !crate::audio::pcm::depth_is_supported(w.audio_bits) {
            w.audio_bits = crate::audio::pcm::BITS_16;
        }
        if w.audio_frame_us != 0 {
            w.audio_frame_us = w.audio_frame_us.max(1_000);
        }
        Ok(ServerHello {
            welcome: w,
            session_id: session_id.ok_or_else(bad)?,
            clock_origin_ns,
            suite,
            features,
        })
    }
}

/// `client → host`: reassembler and decoder exist; send the first frame.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Ready {}

impl V2Message for Ready {
    const TYPE: u64 = reg::MSG_READY;

    fn fields(&self) -> Fields {
        Fields::new()
    }

    fn from_body(body: &[u8]) -> Result<Self> {
        let mut r = FieldReader::new(body);
        while r.next_field()?.is_some() {}
        Ok(Ready {})
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn hello_strategy() -> impl Strategy<Value = Hello> {
        (
            (
                any::<u32>(),
                any::<u32>(),
                any::<u32>(),
                0u8..9,
                0u8..20,
                any::<u32>(),
            ),
            (
                proptest::option::of("[a-zé ]{0,70}"),
                proptest::option::of("[a-z:0-9]{0,130}"),
                any::<u8>(),
                any::<u8>(),
                any::<u8>(),
                any::<u8>(),
            ),
            (
                proptest::option::of(any::<[u16; 4]>()),
                any::<u8>(),
                any::<u16>(),
                any::<u32>(),
                any::<u8>(),
                any::<u8>(),
                any::<u8>(),
            ),
        )
            .prop_map(|(a, b, c)| Hello {
                mode: Mode {
                    width: a.0,
                    height: a.1,
                    refresh_hz: a.2,
                },
                compositor: CompositorPref::from_u8(a.3),
                gamepad: GamepadPref::from_u8(a.4),
                bitrate_kbps: a.5,
                name: b.0,
                launch: b.1,
                video_caps: b.2,
                audio_channels: b.3,
                video_codecs: b.4,
                preferred_codec: b.5,
                display_hdr: c.0.map(|p| HdrMeta {
                    display_primaries: [[p[0], p[1]], [p[2], p[3]], [p[0], p[3]]],
                    white_point: [p[1], p[2]],
                    max_display_mastering_luminance: u32::from(p[0]) << 4,
                    min_display_mastering_luminance: 5,
                    max_cll: p[3],
                    max_fall: p[2],
                }),
                // `CLIENT_CAP_EXT`'s neighbour 0x80 is the last free bit; any byte works.
                client_caps: c.1,
                max_shard_payload: c.2,
                audio_rate_hz: c.3,
                audio_bits: c.4,
                audio_layout: c.5,
                video_fit: c.6,
            })
    }

    fn welcome_strategy() -> impl Strategy<Value = Welcome> {
        (
            (
                any::<u32>(),
                any::<u32>(),
                any::<u32>(),
                any::<u8>(),
                any::<u16>(),
                any::<u16>(),
            ),
            (
                any::<u32>(),
                0u8..9,
                0u8..20,
                any::<u32>(),
                any::<u8>(),
                any::<[u8; 4]>(),
            ),
            (
                any::<u8>(),
                any::<u8>(),
                any::<u8>(),
                any::<u8>(),
                any::<u16>(),
                any::<u32>(),
            ),
            (
                any::<u32>(),
                any::<u8>(),
                any::<u32>(),
                any::<u8>(),
                any::<u16>(),
                any::<u8>(),
                any::<u8>(),
            ),
        )
            .prop_map(|(a, b, c, mut d)| {
                // Opus is 48 kHz / 16-bit with no stated frame, which v1 leaves off the wire.
                if d.1 == AUDIO_CODEC_OPUS {
                    (d.2, d.3, d.4) = (crate::audio::SAMPLE_RATE_HZ, crate::audio::pcm::BITS_16, 0);
                }
                Welcome {
                    mode: Mode {
                        width: a.0,
                        height: a.1,
                        refresh_hz: a.2,
                    },
                    fec: FecConfig {
                        scheme: FecScheme::Gf16,
                        fec_percent: a.3,
                        max_data_per_block: a.4,
                    },
                    shard_payload: a.5,
                    frames: b.0,
                    compositor: CompositorPref::from_u8(b.1),
                    gamepad: GamepadPref::from_u8(b.2),
                    bitrate_kbps: b.3,
                    bit_depth: b.4,
                    color: ColorInfo {
                        primaries: b.5[0],
                        transfer: b.5[1],
                        matrix: b.5[2],
                        full_range: b.5[3],
                    },
                    chroma_format: c.0,
                    audio_channels: c.1,
                    codec: c.2,
                    host_caps: c.3,
                    mgmt_port: c.4,
                    grants: c.5,
                    expires_in_secs: d.0,
                    audio_codec: d.1,
                    audio_rate_hz: d.2,
                    audio_bits: d.3,
                    audio_frame_us: d.4,
                    host_caps2: d.5,
                    audio_layout: d.6,
                }
            })
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(512))]

        /// One trip folds a `Hello` or `Welcome` to what the far side acts on; a second trip
        /// changes nothing.
        #[test]
        fn hellos_settle_after_one_trip(h in hello_strategy(), w in welcome_strategy()) {
            let ch = ClientHello { hello: h, start_ext: vec![], resume: None, suites: vec![], features: FeatureSet::default() };
            let once = ClientHello::from_body(&ch.fields().into_body()).unwrap();
            let twice = ClientHello::from_body(&once.fields().into_body()).unwrap();
            prop_assert_eq!(twice, once);

            let sh = ServerHello { welcome: w, session_id: [7; 16], clock_origin_ns: 1, suite: None, features: FeatureSet::default() };
            let once = ServerHello::from_body(&sh.fields().into_body()).unwrap();
            let twice = ServerHello::from_body(&once.fields().into_body()).unwrap();
            prop_assert_eq!(twice, once);
        }

        /// Hostile bytes never panic either hello decoder.
        #[test]
        fn hello_decoders_survive_any_bytes(body in proptest::collection::vec(any::<u8>(), 0..300)) {
            let _ = ClientHello::from_body(&body);
            let _ = ServerHello::from_body(&body);
        }
    }

    #[test]
    fn start_extensions_and_session_fields_ride_along() {
        let preset = SessionPreset::new("p1", "Couch").unwrap().encode();
        let ch = ClientHello {
            hello: Hello {
                mode: Mode {
                    width: 1920,
                    height: 1080,
                    refresh_hz: 60,
                },
                compositor: CompositorPref::Auto,
                gamepad: GamepadPref::Auto,
                bitrate_kbps: 0,
                name: Some("Deck".into()),
                launch: None,
                video_caps: VIDEO_CAP_CHACHA20,
                audio_channels: 2,
                video_codecs: CODEC_HEVC,
                preferred_codec: 0,
                display_hdr: None,
                client_caps: CLIENT_CAP_EXT,
                max_shard_payload: 1408,
                audio_rate_hz: crate::audio::SAMPLE_RATE_HZ,
                audio_bits: crate::audio::pcm::BITS_16,
                audio_layout: 0,
                video_fit: 0,
            },
            start_ext: vec![
                (EXT_TAG_CLIENT, b"android 0.43".to_vec()),
                (EXT_TAG_ABR, vec![EXT_ABR_ACK_REASON]),
                (EXT_TAG_PRESET, preset.clone()),
                (
                    EXT_TAG_DELIVERY,
                    DeliveryAsk {
                        profile: 2,
                        flags: EXT_DELIVERY_FACTS,
                    }
                    .encode()
                    .to_vec(),
                ),
            ],
            resume: Some([3; 16]),
            suites: vec![MediaSuite::ChaCha20Poly1305, MediaSuite::Aes128Gcm],
            features: FeatureSet::default().with(reg::FEATURE_STREAM_CONFIG),
        };
        let back = ClientHello::from_body(&ch.fields().into_body()).unwrap();
        assert_eq!(back, ch);
        let entries = back.ext_entries();
        assert_eq!(ext_abr_features(&entries), EXT_ABR_ACK_REASON);
        assert_eq!(SessionPreset::from_ext(&entries).unwrap().name, "Couch");
        assert_eq!(DeliveryAsk::from_ext(&entries).unwrap().profile, 2);

        let sh = ServerHello {
            welcome: ServerHello::from_body(&Fields::new().bytes(1, &[0; 16]).into_body())
                .unwrap()
                .welcome,
            session_id: [9; 16],
            clock_origin_ns: 1_700_000_000_000_000_000,
            suite: Some(MediaSuite::ChaCha20Poly1305),
            features: FeatureSet::default().with(reg::FEATURE_STREAM_CONFIG),
        };
        assert_eq!(
            ServerHello::from_body(&sh.fields().into_body()).unwrap(),
            sh
        );
        // A ServerHello without a session id, or with a suite this build lacks, is refused.
        assert!(ServerHello::from_body(&Fields::new().u64(2, 1).into_body()).is_err());
        let unknown_suite = Fields::new().bytes(1, &[0; 16]).u8(3, 9).into_body();
        assert!(ServerHello::from_body(&unknown_suite).is_err());
        assert_eq!(
            Ready::from_body(&Ready {}.fields().into_body()).unwrap(),
            Ready {}
        );
    }
}
