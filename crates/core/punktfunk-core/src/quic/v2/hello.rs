//! The handshake: `ClientHello` → `ServerHello` → `Ready`.
//!
//! A [`ClientHello`] decodes to a [`Hello`] plus what the client adds beside it, a
//! [`ServerHello`] to a [`Welcome`]. Decoding folds what this build cannot honour onto its
//! default (unknown codec to HEVC, unsupported rate to 48 kHz, bad name to none), so host and
//! client logic only sees values it can act on; `tests::hellos_settle_after_one_trip` pins that
//! one trip settles both.
//!
//! The hellos carry no wire version (ALPN picks the wire), no data port (media rides the QUIC
//! path) and no session key (both ends derive it).

use super::features::FeatureSet;
use super::field::*;
use super::msg::{FieldValue, V2Message, PROFILE_ID_MAX};
use super::registry as reg;
use crate::config::{CompositorPref, FecConfig, FecScheme, GamepadPref, Mode};
use crate::crypto::MediaSuite;
use crate::error::Result;
use crate::quic::*;

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
    /// What the client calls itself ([`client_label`]): its build and the shell that dialled
    /// (`"android 0.38.0 console/library"`). A label for the host's log, never a fact it acts
    /// on: two sessions from one device are told apart here instead of by capture.
    pub client_label: Option<String>,
    /// ABR protocol features the client reads, one bit each ([`EXT_ABR_ACK_REASON`] is bit 0).
    /// A later feature takes another bit rather than a field of its own; `0` reads none.
    pub abr_features: u8,
    /// The settings preset this session was dialled with; `None` streams with plain settings.
    pub preset: Option<SessionPreset>,
    /// What this client's OS says about its end of the path. The default is unknown.
    pub link: LinkFacts,
    /// A diagnostic session: serve probes from the bound data plane, never build a pipeline.
    pub probe_only: bool,
    /// The player's PyroWave quality in hundredths of a bit per pixel; `0` leaves the host's.
    pub pyrowave_bpp_x100: u16,
    /// H.264/HEVC slices per picture this client's decoder wants, read only under
    /// [`VIDEO_CAP_MULTI_SLICE`](crate::quic::VIDEO_CAP_MULTI_SLICE); `0` leaves the host's.
    pub slices: u8,
    /// The session this client held, to take back after a drop.
    pub resume: Option<[u8; 16]>,
    /// Media AEADs the client takes, most wanted first. Empty on a carrier that encrypts.
    pub suites: Vec<MediaSuite>,
    /// Bits from 32 up ([`FeatureSet::native`]); `hello`'s capability bytes fill 0–31.
    pub features: FeatureSet,
    /// The profile this device asks to play as. `None` asks nothing.
    pub profile: Option<String>,
    /// Ask for access only: the host closes with [`crate::reject::ACCESS_GRANTED_CLOSE_CODE`]
    /// once this device may connect, and starts no session. An older host streams instead.
    pub access_only: bool,
    /// Every screen this client shows, screen 0 first and matching `hello.mode`; empty for
    /// one. Under [`reg::FEATURE_SCREENS`]. The further screens' views ride as tag 31.
    pub screens: Vec<super::msg::Screen>,
    /// The size the client shows the picture at, when smaller than `hello.mode`: the host builds
    /// the display at the mode and scales the picture to this before encoding. `None` encodes
    /// the mode.
    pub view: Option<(u32, u32)>,
    /// The screen a preset asks for; `None` leaves it to the host card's pick.
    pub screen_wish: Option<ScreenWish>,
}

impl ClientHello {
    /// A `ProfileUnlock` follows this hello on the same stream ([`reg::FEATURE_UNLOCK_FOLLOWS`]):
    /// read it with `FrameReader::read_profile_unlock` before anything else. An older client's
    /// hello reads `false`.
    pub fn unlock_follows(&self) -> bool {
        self.features.has(reg::FEATURE_UNLOCK_FOLLOWS)
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
        // A default value is left out: absence reads back as the same thing.
        let label = self.client_label.as_deref().filter(|l| !l.is_empty());
        let preset = self.preset.as_ref().map(SessionPreset::encode);
        let link = (self.link != LinkFacts::default()).then(|| self.link.encode());
        // An id is never truncated: a cut one would name another profile or none.
        let profile = self
            .profile
            .as_deref()
            .filter(|p| p.len() <= PROFILE_ID_MAX);
        let views = super::msg::screen_views(&self.screens);
        // Tag 24 goes before 23; `tests::start_extensions_and_session_fields_ride_along` pins it.
        f.when(label.is_some(), |f| f.str(19, label.unwrap_or("")))
            .when(self.abr_features != 0, |f| f.u8(20, self.abr_features))
            .when(preset.is_some(), |f| {
                f.bytes(21, preset.as_deref().unwrap_or_default())
            })
            .when(link.is_some(), |f| f.bytes(22, &link.unwrap_or_default()))
            .when(self.probe_only, |f| f.u8(24, 1))
            .when(self.pyrowave_bpp_x100 != 0, |f| {
                f.bytes(25, &self.pyrowave_bpp_x100.to_le_bytes())
            })
            .when(profile.is_some(), |f| f.str(23, profile.unwrap_or("")))
            .when(self.access_only, |f| f.u8(26, 1))
            .when(!self.screens.is_empty(), |f| self.screens.put(f, 27))
            .when(self.slices != 0, |f| f.u8(28, self.slices))
            .when(h.audio_bitstreams != 0, |f| f.u8(29, h.audio_bitstreams))
            .when(self.view.is_some(), |f| self.view.put(f, 30))
            .when(views.is_some(), |f| {
                f.bytes(31, views.as_deref().unwrap_or_default())
            })
            .when(self.screen_wish.is_some(), |f| {
                let wish = self.screen_wish.as_ref().map(ScreenWish::wire);
                f.bytes(32, &wish.unwrap_or_default())
            })
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
            audio_bitstreams: 0,
        };
        let (mut resume, mut suites) = (None, Vec::new());
        let (mut client_label, mut abr_features, mut preset) = (None, 0, None);
        let (mut link, mut probe_only) = (LinkFacts::default(), false);
        let (mut pyrowave_bpp_x100, mut slices) = (0u16, 0u8);
        let mut features = FeatureSet::default();
        let (mut profile, mut access_only) = (None, false);
        let (mut screens, mut view, mut views) = (Vec::new(), None, None);
        let mut screen_wish = None;
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
                // What the client adds informs and never gates: a bad value is its default.
                19 => {
                    client_label = Some(crate::quic::client_label(&String::from_utf8_lossy(v)))
                        .filter(|l| !l.is_empty())
                }
                20 => abr_features = v.first().copied().unwrap_or(0),
                21 => preset = SessionPreset::decode(v),
                22 => link = LinkFacts::decode(v),
                23 => profile = label(v, PROFILE_ID_MAX),
                24 => probe_only = true,
                // A short value is no ask, never a failed handshake.
                25 => {
                    pyrowave_bpp_x100 = v
                        .get(..2)
                        .and_then(|b| b.try_into().ok())
                        .map(u16::from_le_bytes)
                        .unwrap_or(0)
                }
                26 => access_only = v.first() == Some(&1),
                27 => screens = <Vec<super::msg::Screen> as FieldValue>::get(v).unwrap_or_default(),
                28 => slices = v.first().copied().unwrap_or(0),
                29 => h.audio_bitstreams = u8_of(v)?,
                30 => view = <Option<(u32, u32)> as FieldValue>::get(v)?,
                31 => views = Some(v),
                32 => screen_wish = ScreenWish::of_wire(v),
                _ => {}
            }
        }
        super::msg::read_screen_views(&mut screens, views.unwrap_or_default());
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
            client_label,
            abr_features,
            preset,
            link,
            probe_only,
            pyrowave_bpp_x100,
            slices,
            resume,
            suites,
            features,
            profile,
            access_only,
            screens,
            view,
            screen_wish,
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

/// Longest connector a [`ScreenEcho::Monitor`] carries; a longer one reads as no echo.
const CONNECTOR_MAX: usize = 64;

/// What a session streams, as its `ServerHello` says: a screen of its own, the monitor it
/// mirrors by connector (`HDMI-A-1`), or a screen another device opened. Tag 28 carries `0x00`,
/// `0xFF`, else the connector.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScreenEcho {
    /// A virtual display the host made for this session.
    Own,
    /// A physical monitor, by connector.
    Monitor(String),
    /// The screen of the session that connected first, sized by it.
    Shared,
}

/// [`ScreenEcho::Shared`] on the wire. Not UTF-8, so an older build reads it as no echo.
const SHARED_WIRE: u8 = 0xFF;

impl ScreenEcho {
    fn wire(&self) -> &[u8] {
        match self {
            ScreenEcho::Own => &[0],
            ScreenEcho::Monitor(c) => c.as_bytes(),
            ScreenEcho::Shared => &[SHARED_WIRE],
        }
    }

    fn of_wire(v: &[u8]) -> Option<Self> {
        match v {
            [0] => Some(ScreenEcho::Own),
            [SHARED_WIRE] => Some(ScreenEcho::Shared),
            _ => label(v, CONNECTOR_MAX).map(ScreenEcho::Monitor),
        }
    }
}

/// How a preset asks to use the host's screens: one of the three roles, or a copy of a monitor
/// by connector. The host weighs it only for a device it lets choose; a wish it can't honour is
/// not made. Spelled `only`, `main`, `second` or `monitor:<connector>`; tag 32 carries `1`,
/// `2`, `3`, or `4` and the connector.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScreenWish {
    Only,
    Main,
    Second,
    Monitor(String),
}

impl ScreenWish {
    /// The preset spelling. Anything else, and an empty or oversized connector, is no wish.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "only" => Some(ScreenWish::Only),
            "main" => Some(ScreenWish::Main),
            "second" => Some(ScreenWish::Second),
            _ => label(s.strip_prefix("monitor:")?.as_bytes(), CONNECTOR_MAX)
                .map(ScreenWish::Monitor),
        }
    }

    fn wire(&self) -> Vec<u8> {
        match self {
            ScreenWish::Only => vec![1],
            ScreenWish::Main => vec![2],
            ScreenWish::Second => vec![3],
            ScreenWish::Monitor(c) => [&[4], c.as_bytes()].concat(),
        }
    }

    fn of_wire(v: &[u8]) -> Option<Self> {
        match v {
            [1] => Some(ScreenWish::Only),
            [2] => Some(ScreenWish::Main),
            [3] => Some(ScreenWish::Second),
            [4, c @ ..] => label(c, CONNECTOR_MAX).map(ScreenWish::Monitor),
            _ => None,
        }
    }
}

/// Writes the spelling [`ScreenWish::parse`] reads.
impl std::fmt::Display for ScreenWish {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ScreenWish::Only => f.write_str("only"),
            ScreenWish::Main => f.write_str("main"),
            ScreenWish::Second => f.write_str("second"),
            ScreenWish::Monitor(c) => write!(f, "monitor:{c}"),
        }
    }
}

/// A session's role among the host's screens: the only one, the main one beside the host's
/// monitors, or one more beside them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(rename_all = "lowercase")
)]
pub enum Role {
    Only,
    Main,
    Second,
}

impl Role {
    pub fn to_u8(self) -> u8 {
        match self {
            Role::Only => 1,
            Role::Main => 2,
            Role::Second => 3,
        }
    }

    /// `0` is no role, so it reads as `None` like an unknown byte.
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(Role::Only),
            2 => Some(Role::Main),
            3 => Some(Role::Second),
            _ => None,
        }
    }
}

/// Why a session's screen is not the one its device asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(rename_all = "snake_case")
)]
pub enum RoleReason {
    AsAsked,
    /// The device may not choose its screen on this host.
    NotAllowed,
    /// An earlier session decided what the host's monitors do.
    MonitorsHeld,
    /// The monitor asked for is off, taken or missing.
    Unavailable,
    /// This desktop has no monitors.
    NoMonitors,
}

impl RoleReason {
    pub fn to_u8(self) -> u8 {
        match self {
            RoleReason::AsAsked => 0,
            RoleReason::NotAllowed => 1,
            RoleReason::MonitorsHeld => 2,
            RoleReason::Unavailable => 3,
            RoleReason::NoMonitors => 4,
        }
    }

    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(RoleReason::AsAsked),
            1 => Some(RoleReason::NotAllowed),
            2 => Some(RoleReason::MonitorsHeld),
            3 => Some(RoleReason::Unavailable),
            4 => Some(RoleReason::NoMonitors),
            _ => None,
        }
    }
}

/// The role a session got and why it differs from the one asked, as its `ServerHello` says.
/// Tag 30 carries the role byte (`0` none) and the reason byte; anything else is no echo.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RoleEcho {
    /// `None` when the session has no role: it copies a monitor, or the host has none.
    pub role: Option<Role>,
    pub reason: RoleReason,
}

impl RoleEcho {
    fn wire(self) -> [u8; 2] {
        [self.role.map_or(0, Role::to_u8), self.reason.to_u8()]
    }

    fn of_wire(v: &[u8]) -> Option<Self> {
        let &[role, reason] = v else { return None };
        Some(RoleEcho {
            role: match role {
                0 => None,
                r => Some(Role::from_u8(r)?),
            },
            reason: RoleReason::from_u8(reason)?,
        })
    }
}

/// `host → client`: the session.
#[derive(Clone, Debug, PartialEq, Eq)]
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
    /// The profile this session resolved to. `None` from a host without profiles.
    pub profile: Option<String>,
    /// The screen this session streams. `None` from a host that does not say.
    pub screen: Option<ScreenEcho>,
    /// The size the host encodes the first screen at when it scales the mode down for the
    /// client's view. `None` encodes the mode, as an older host always does.
    pub picture: Option<(u32, u32)>,
    /// The role this session got among the host's screens. `None` from a host that does not say.
    pub role: Option<RoleEcho>,
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
        f = f
            .u8(6, w.fec.scheme as u8)
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
            .when(self.profile.is_some(), |f| {
                f.str(27, self.profile.as_deref().unwrap_or(""))
            })
            .when(self.screen.is_some(), |f| {
                f.bytes(28, self.screen.as_ref().map_or(&[][..], ScreenEcho::wire))
            });
        self.picture.put(f, 29).when(self.role.is_some(), |f| {
            f.bytes(30, &self.role.map_or([0; 2], RoleEcho::wire))
        })
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
        let (mut profile, mut screen, mut picture, mut role) = (None, None, None, None);
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
                27 => profile = label(v, PROFILE_ID_MAX),
                28 => screen = ScreenEcho::of_wire(v),
                29 => picture = <Option<(u32, u32)> as FieldValue>::get(v)?,
                30 => role = RoleEcho::of_wire(v),
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
            profile,
            screen,
            picture,
            role,
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
                // Any byte works: an unknown bit rides through.
                client_caps: c.1,
                max_shard_payload: c.2,
                audio_rate_hz: c.3,
                audio_bits: c.4,
                audio_layout: c.5,
                video_fit: c.6,
                audio_bitstreams: c.7,
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
            let ch = ClientHello { hello: h, client_label: None, abr_features: 0, preset: None, link: LinkFacts::default(), probe_only: false, pyrowave_bpp_x100: 0, slices: 0, resume: None, suites: vec![], features: FeatureSet::default(), profile: None, access_only: false, screens: vec![], view: None, screen_wish: None };
            let once = ClientHello::from_body(&ch.fields().into_body()).unwrap();
            let twice = ClientHello::from_body(&once.fields().into_body()).unwrap();
            prop_assert_eq!(twice, once);

            let sh = ServerHello { welcome: w, session_id: [7; 16], clock_origin_ns: 1, suite: None, features: FeatureSet::default(), profile: None, screen: None, picture: None, role: None };
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

    /// The ask rides `ClientHello` tag 23 with the `PROFILES` bit, the echo `ServerHello` tag 27.
    /// An id over [`PROFILE_ID_MAX`] is never sent, and one that arrives is no ask.
    #[test]
    fn a_profile_rides_client_hello_and_echoes() {
        let mut ch = ClientHello::from_body(&Fields::new().into_body()).unwrap();
        ch.profile = Some("9a3f1c2b7e40".into());
        ch.features = FeatureSet::default().with(reg::FEATURE_PROFILES);
        let back = ClientHello::from_body(&ch.fields().into_body()).unwrap();
        assert_eq!(back.profile.as_deref(), Some("9a3f1c2b7e40"));
        assert!(back.features.has(reg::FEATURE_PROFILES));

        ch.profile = Some("9".repeat(PROFILE_ID_MAX + 1));
        let body = ch.fields().into_body();
        assert_eq!(ClientHello::from_body(&body).unwrap().profile, None);
        let oversized = Fields::new()
            .str(23, &"9".repeat(PROFILE_ID_MAX + 1))
            .into_body();
        assert_eq!(ClientHello::from_body(&oversized).unwrap().profile, None);

        let mut sh = ServerHello::from_body(&Fields::new().bytes(1, &[0; 16]).into_body()).unwrap();
        assert_eq!(sh.profile, None);
        sh.profile = Some("9a3f1c2b7e40".into());
        let back = ServerHello::from_body(&sh.fields().into_body()).unwrap();
        assert_eq!(back.profile.as_deref(), Some("9a3f1c2b7e40"));
    }

    /// The screen echo rides `ServerHello` tag 28, after the profile: `0x00` for the session's
    /// own screen, else the connector. A host that says nothing, or an empty or oversized
    /// value, reads as no echo.
    #[test]
    fn the_screen_echo_rides_server_hello() {
        let hex = |sh: &ServerHello| -> String {
            let body = sh.fields().into_body();
            body.iter().map(|b| format!("{b:02x}")).collect()
        };
        let mut sh = ServerHello::from_body(&Fields::new().bytes(1, &[0; 16]).into_body()).unwrap();
        assert_eq!(sh.screen, None);
        sh.profile = Some("p1".into());
        assert!(hex(&sh).ends_with("1b027031"), "no echo, no tag 28");
        // The bytes on the wire are pinned: a change here is a wire change.
        sh.screen = Some(ScreenEcho::Own);
        assert!(hex(&sh).ends_with("1b0270311c0100"), "{}", hex(&sh));
        assert_eq!(
            ServerHello::from_body(&sh.fields().into_body()).unwrap(),
            sh
        );
        sh.screen = Some(ScreenEcho::Monitor("HDMI-A-1".into()));
        assert!(
            hex(&sh).ends_with("1b0270311c0848444d492d412d31"),
            "{}",
            hex(&sh)
        );
        assert_eq!(
            ServerHello::from_body(&sh.fields().into_body()).unwrap(),
            sh
        );

        let read = |v: &[u8]| {
            let body = Fields::new().bytes(1, &[0; 16]).bytes(28, v).into_body();
            ServerHello::from_body(&body).unwrap().screen
        };
        assert_eq!(read(b"DP-1"), Some(ScreenEcho::Monitor("DP-1".into())));
        assert_eq!(read(&[]), None);
        assert_eq!(read(&[b'x'; CONNECTOR_MAX + 1]), None);
    }

    /// A shared screen rides tag 28 as `0xFF`. A build without the variant reads every value but
    /// `0x00` as a connector label, and `0xFF` is none, so it sees no echo, as for a joiner before.
    #[test]
    fn a_shared_screen_rides_the_screen_echo() {
        let mut sh = ServerHello::from_body(&Fields::new().bytes(1, &[0; 16]).into_body()).unwrap();
        sh.screen = Some(ScreenEcho::Shared);
        let body = sh.fields().into_body();
        assert!(body.ends_with(&[0x1c, 0x01, 0xff]), "{body:02x?}");
        assert_eq!(ServerHello::from_body(&body).unwrap(), sh);
        assert_eq!(
            label(&[SHARED_WIRE], CONNECTOR_MAX),
            None,
            "an older build's read"
        );
    }

    /// A wish reads and writes the preset spelling and rides tag 32 as one byte, or `4` and the
    /// connector. Anything else is no wish, never a failed handshake.
    #[test]
    fn a_screen_wish_rides_client_hello() {
        let wishes = [
            ("only", ScreenWish::Only, &[1u8][..]),
            ("main", ScreenWish::Main, &[2]),
            ("second", ScreenWish::Second, &[3]),
            (
                "monitor:HDMI-A-1",
                ScreenWish::Monitor("HDMI-A-1".into()),
                b"\x04HDMI-A-1",
            ),
        ];
        for (spelled, wish, wire) in wishes {
            assert_eq!(ScreenWish::parse(spelled).as_ref(), Some(&wish));
            assert_eq!(wish.to_string(), spelled);
            let mut ch = ClientHello::from_body(&Fields::new().into_body()).unwrap();
            ch.screen_wish = Some(wish);
            let body = ch.fields().into_body();
            let field = [&[32, wire.len() as u8][..], wire].concat();
            assert!(body.ends_with(&field), "{spelled}: {body:02x?}");
            assert_eq!(ClientHello::from_body(&body).unwrap(), ch);
        }
        let long = format!("monitor:{}", "x".repeat(CONNECTOR_MAX + 1));
        for nothing in ["", "Only", "monitor", "monitor:", "other", long.as_str()] {
            assert_eq!(ScreenWish::parse(nothing), None, "{nothing:?}");
        }

        let read = |v: &[u8]| {
            let body = Fields::new().bytes(32, v).into_body();
            ClientHello::from_body(&body).unwrap().screen_wish
        };
        let oversized = [&[4u8][..], &[b'x'; CONNECTOR_MAX + 1]].concat();
        for bad in [&[][..], &[0], &[5], &[1, 1], &[4], &[4, 0xFF], &oversized] {
            assert_eq!(read(bad), None, "{bad:02x?}");
        }
        assert_eq!(
            ClientHello::from_body(&Fields::new().into_body())
                .unwrap()
                .screen_wish,
            None
        );
    }

    /// The role echo rides `ServerHello` tag 30 as a role byte and a reason byte. Any other
    /// value is no echo, never a refused session.
    #[test]
    fn the_role_echo_rides_server_hello() {
        let mut sh = ServerHello::from_body(&Fields::new().bytes(1, &[0; 16]).into_body()).unwrap();
        assert_eq!(sh.role, None);
        sh.profile = Some("p1".into());
        sh.role = Some(RoleEcho {
            role: Some(Role::Second),
            reason: RoleReason::MonitorsHeld,
        });
        let body = sh.fields().into_body();
        assert!(
            body.ends_with(&[0x1b, 0x02, b'p', b'1', 0x1e, 0x02, 3, 2]),
            "{body:02x?}"
        );
        assert_eq!(ServerHello::from_body(&body).unwrap(), sh);

        for v in 0..=u8::MAX {
            assert_eq!(
                Role::from_u8(v).map(Role::to_u8),
                (1..=3).contains(&v).then_some(v)
            );
            let reason = RoleReason::from_u8(v).map(RoleReason::to_u8);
            assert_eq!(reason, (v <= 4).then_some(v));
        }
        let read = |v: &[u8]| {
            let body = Fields::new().bytes(1, &[0; 16]).bytes(30, v).into_body();
            ServerHello::from_body(&body).unwrap().role
        };
        let no_monitors = RoleEcho {
            role: None,
            reason: RoleReason::NoMonitors,
        };
        assert_eq!(read(&[0, 4]), Some(no_monitors));
        for bad in [&[][..], &[1], &[1, 0, 0], &[4, 0], &[1, 5]] {
            assert_eq!(read(bad), None, "{bad:02x?}");
        }
    }

    /// A tag this build does not know is skipped, so an older host reads past tag 32 and an
    /// older client past tag 30, as this one reads past 33.
    #[test]
    fn an_unknown_tag_is_skipped() {
        let ch = Fields::new().u8(10, 6).bytes(33, &[9, 9]).into_body();
        assert_eq!(ClientHello::from_body(&ch).unwrap().hello.audio_channels, 6);
        let sh = Fields::new().bytes(1, &[0; 16]).bytes(31, &[9]).into_body();
        assert_eq!(ServerHello::from_body(&sh).unwrap().session_id, [0; 16]);
    }

    /// The unlock promise rides the feature bits; a hello without it reads `false`.
    #[test]
    fn an_unlock_promise_rides_the_hello() {
        let mut ch = ClientHello::from_body(&Fields::new().into_body()).unwrap();
        assert!(!ch.unlock_follows());
        ch.features = ch.features.with(reg::FEATURE_UNLOCK_FOLLOWS);
        let back = ClientHello::from_body(&ch.fields().into_body()).unwrap();
        assert!(back.unlock_follows());
    }

    /// What a client adds is left out at its default, and a value this build cannot use reads
    /// as that default instead of failing the handshake.
    #[test]
    fn client_additions_skip_defaults_and_fold_bad_values() {
        let tags = |ch: &ClientHello| {
            let body = ch.fields().into_body();
            let mut r = FieldReader::new(&body);
            let mut tags = Vec::new();
            while let Some((tag, _)) = r.next_field().unwrap() {
                tags.push(tag);
            }
            tags
        };
        let mut ch = ClientHello::from_body(&Fields::new().into_body()).unwrap();
        ch.client_label = Some(String::new());
        assert!(tags(&ch).iter().all(|t| !(19..=25).contains(t)));
        ch.probe_only = true;
        assert!(tags(&ch).contains(&24));
        ch.pyrowave_bpp_x100 = 120;
        assert!(tags(&ch).contains(&25));
        assert!(!tags(&ch).contains(&28));
        ch.slices = 8;
        assert!(tags(&ch).contains(&28));
        assert!(!tags(&ch).contains(&29));
        ch.hello.audio_bitstreams = 0x03;
        assert!(tags(&ch).contains(&29));
        assert!(!tags(&ch).contains(&30));
        ch.view = Some((1366, 768));
        assert!(tags(&ch).contains(&30));

        let read = |tag, v: &[u8]| {
            ClientHello::from_body(&Fields::new().bytes(tag, v).into_body()).unwrap()
        };
        // A bit this build does not know is ignored; bit 0 still reads.
        let future = read(20, &[EXT_ABR_ACK_REASON | 0xF0]);
        assert_eq!(future.abr_features & EXT_ABR_ACK_REASON, EXT_ABR_ACK_REASON);
        assert_eq!(read(20, &[]).abr_features, 0);
        assert_eq!(read(19, b"  deck\n").client_label.as_deref(), Some("deck"));
        assert_eq!(read(19, b"\n").client_label, None);
        assert_eq!(read(21, &[9, b'x']).preset, None);
        assert!(read(24, &[]).probe_only);
        assert_eq!(read(25, &[120, 0]).pyrowave_bpp_x100, 120);
        // A short value is no ask, never a failed handshake.
        assert_eq!(read(25, &[7]).pyrowave_bpp_x100, 0);
        assert_eq!(read(28, &[8]).slices, 8);
        assert_eq!(read(28, &[]).slices, 0);
        assert_eq!(read(29, &[0x02]).hello.audio_bitstreams, 0x02);
        assert!(read(26, &[1]).access_only);
        assert!(!read(26, &[0]).access_only);
        assert!(!read(24, &[]).access_only);
        assert_eq!(
            read(30, &[0x56, 5, 0, 0, 0, 3, 0, 0]).view,
            Some((1366, 768))
        );
        assert_eq!(read(30, &[0x56, 5, 0, 0]).view, None);
        assert_eq!(read(30, &[0x56, 5, 0, 0, 0, 0, 0, 0]).view, None);
    }

    #[test]
    fn start_extensions_and_session_fields_ride_along() {
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
                client_caps: 0x40,
                max_shard_payload: 1408,
                audio_rate_hz: crate::audio::SAMPLE_RATE_HZ,
                audio_bits: crate::audio::pcm::BITS_16,
                audio_layout: 0,
                video_fit: 0,
                audio_bitstreams: 0,
            },
            client_label: Some("android 0.43".into()),
            abr_features: EXT_ABR_ACK_REASON,
            preset: SessionPreset::new("p1", "Couch"),
            link: LinkFacts {
                kind: IFACE_KIND_ETHERNET,
                mbps: 2_500,
            },
            probe_only: true,
            pyrowave_bpp_x100: 120,
            slices: 0,
            resume: Some([3; 16]),
            suites: vec![MediaSuite::ChaCha20Poly1305, MediaSuite::Aes128Gcm],
            features: FeatureSet::default().with(reg::FEATURE_STREAM_CONFIG),
            profile: None,
            access_only: false,
            screens: vec![],
            view: None,
            screen_wish: None,
        };
        // The bytes on the wire are pinned: a change here is a wire change.
        let frame: String = ch.encode_v2().iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            frame,
            "01408301100303030303030303030303030303030302020100030c80070000380400003c000000040100\
             05010006040000000007044465636b090540400000010a01020b01020c01000e0280050f0480bb0000\
             100110110100120100130c616e64726f696420302e3433140101150902703105436f756368160501c4\
             09000018010119027800"
        );
        let back = ClientHello::from_body(&ch.fields().into_body()).unwrap();
        assert_eq!(back, ch);

        // A view rides last, as tag 30.
        let viewed = ClientHello {
            view: Some((1366, 768)),
            ..ch.clone()
        };
        let body: String = viewed
            .fields()
            .into_body()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert!(body.ends_with("190278001e085605000000030000"), "{body}");
        assert_eq!(
            ClientHello::from_body(&viewed.fields().into_body()).unwrap(),
            viewed
        );

        // Screens ride along; a torn list reads as one screen, never a failed handshake.
        let two = ClientHello {
            screens: vec![
                super::super::msg::Screen {
                    mode: ch.hello.mode,
                    primary: true,
                    ..Default::default()
                },
                super::super::msg::Screen {
                    index: 1,
                    mode: ch.hello.mode,
                    x: 1920,
                    ..Default::default()
                },
            ],
            ..ch.clone()
        };
        assert_eq!(
            ClientHello::from_body(&two.fields().into_body()).unwrap(),
            two
        );
        let torn = ch.fields().bytes(27, &[1, 2, 3]).into_body();
        assert!(ClientHello::from_body(&torn).unwrap().screens.is_empty());

        // A further screen's view rides last, as tag 31; a torn one reads as none.
        let mut viewed = two.clone();
        viewed.screens[1].view = Some((1280, 720));
        let body = viewed.fields().into_body();
        let records = two.fields().into_body();
        assert_eq!(body[..records.len()], records[..]);
        assert_eq!(
            body[records.len()..],
            [0x1F, 0x0C, 0x01, 0, 0, 0, 0x00, 0x05, 0, 0, 0xD0, 0x02, 0, 0]
        );
        assert_eq!(ClientHello::from_body(&body).unwrap(), viewed);
        let torn = two.fields().bytes(31, &body[records.len() + 2..][..11]);
        assert_eq!(ClientHello::from_body(&torn.into_body()).unwrap(), two);

        // A screen wish rides last, as tag 32: everything before it is the hello without it.
        let wished = ClientHello {
            screen_wish: Some(ScreenWish::Monitor("DP-1".into())),
            ..viewed.clone()
        };
        let body = wished.fields().into_body();
        let plain = viewed.fields().into_body();
        assert_eq!(body[..plain.len()], plain[..]);
        assert_eq!(
            body[plain.len()..],
            [0x20, 0x05, 0x04, b'D', b'P', b'-', b'1']
        );
        assert_eq!(ClientHello::from_body(&body).unwrap(), wished);

        let sh = ServerHello {
            welcome: ServerHello::from_body(&Fields::new().bytes(1, &[0; 16]).into_body())
                .unwrap()
                .welcome,
            session_id: [9; 16],
            clock_origin_ns: 1_700_000_000_000_000_000,
            suite: Some(MediaSuite::ChaCha20Poly1305),
            features: FeatureSet::default().with(reg::FEATURE_STREAM_CONFIG),
            profile: None,
            screen: None,
            picture: Some((1920, 1080)),
            role: None,
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
