//! Host configuration: the [`registry`] rows the web console edits, plus the
//! env-only knobs.
//!
//! [`HostConfig`] is the one resolved value capture, topology, and encoding
//! share. A registry row resolves flag > env > `host-settings.json` > default;
//! a console write swaps the snapshot, so read [`config`] where the value is
//! used, not once at startup. Session-mutated compositor variables, path
//! lookups, credentials, and single-use tuning stay live reads at their call
//! sites. [`env_on`] is the explicit-off grammar of the env-only knobs.
#![forbid(unsafe_code)]

/// Keyboard LAYOUT from `localectl`, not a `PUNKTFUNK_*` knob. Shared so the
/// injector and the gamescope backend do not depend on each other.
pub mod layout;
pub mod registry;
mod store;

pub use store::{
    knob, mark_started, pin, reload, restart_pending, save, snapshot, store_path, Resolved,
    SaveError, Snapshot, Source,
};

/// Explicit-off for a `PUNKTFUNK_*` var: trimmed, case-insensitive
/// `0`/`false`/`off`/`no` are off; any other present value is on; unset is
/// `None`. Callers must use this — `var(k) != Ok("0")` treats `"0 "` and
/// `"false"` as ON.
///
/// Not `pf-zerocopy`'s grammar (`1|true|yes|on` on, everything else off).
///
/// Reads through [`knob`], so a registry row's console value counts as set.
pub fn env_on(name: &str) -> Option<bool> {
    knob(name).map(|s| is_on(&s))
}

fn is_on(s: &str) -> bool {
    !matches!(
        s.trim().to_ascii_lowercase().as_str(),
        "0" | "false" | "off" | "no"
    )
}

/// Which render endpoint the loopback captures (registry row `audio_output_mode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AudioOutputMode {
    /// Silent render endpoint; streamed audio does not also play on the host.
    #[default]
    ClientOnly,
    /// Host hardware plays as well as the client. `PUNKTFUNK_HOST_AUDIO=1`.
    HostAndClient,
    /// Operator's default playback device; never write default-device policy.
    /// `PUNKTFUNK_KEEP_DEFAULT=1`.
    FollowDefault,
}

impl AudioOutputMode {
    pub fn parse(s: &str) -> Option<AudioOutputMode> {
        match s.trim().to_ascii_lowercase().replace('-', "_").as_str() {
            "client_only" | "client" => Some(AudioOutputMode::ClientOnly),
            "host_and_client" | "both" | "host" => Some(AudioOutputMode::HostAndClient),
            "follow_default" | "follow" => Some(AudioOutputMode::FollowDefault),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            AudioOutputMode::ClientOnly => "client_only",
            AudioOutputMode::HostAndClient => "host_and_client",
            AudioOutputMode::FollowDefault => "follow_default",
        }
    }

    pub fn prefers_host_hardware(self) -> bool {
        matches!(self, AudioOutputMode::HostAndClient)
    }

    pub fn keeps_default(self) -> bool {
        matches!(self, AudioOutputMode::FollowDefault)
    }
}

/// Where voice-chat apps play while a stream runs (registry row `audio_voice_chat`).
/// Linux moves the apps' PipeWire streams; Windows writes their per-app output device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VoiceChatRoute {
    /// In the stream like everything else. Right when the viewer is not in the call.
    #[default]
    Stream,
    /// On the host's own output, out of the stream: friends in the call never hear
    /// their own voices back.
    Host,
}

impl VoiceChatRoute {
    pub fn parse(s: &str) -> Option<VoiceChatRoute> {
        match s.trim().to_ascii_lowercase().as_str() {
            "stream" | "client" => Some(VoiceChatRoute::Stream),
            "host" | "speakers" => Some(VoiceChatRoute::Host),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            VoiceChatRoute::Stream => "stream",
            VoiceChatRoute::Host => "host",
        }
    }
}

/// Whether the host shares its clipboard (registry row `clipboard`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ClipboardPolicy {
    #[default]
    Off,
    Text,
    /// Text and files.
    Files,
}

impl ClipboardPolicy {
    pub fn parse(s: &str) -> Option<ClipboardPolicy> {
        match s {
            "off" => Some(ClipboardPolicy::Off),
            "text" => Some(ClipboardPolicy::Text),
            "files" => Some(ClipboardPolicy::Files),
            _ => None,
        }
    }
}

/// Lowercase fragments a voice-chat app's `application.name`, process binary or
/// exe file name contains. Discord's three builds all contain `discord`.
pub const DEFAULT_VOICE_APPS: &[&str] = &[
    "discord",
    "vesktop",
    "webcord",
    "armcord",
    "legcord",
    "teamspeak",
    "ts3client",
    "mumble",
];

/// `PUNKTFUNK_AUDIO_VOICE_APPS`: a comma list added to [`DEFAULT_VOICE_APPS`],
/// lowercased, blanks and repeats dropped. A console store feeds the same list later.
pub fn parse_voice_apps(raw: Option<&str>) -> Vec<String> {
    let mut apps: Vec<String> = DEFAULT_VOICE_APPS.iter().map(|s| s.to_string()).collect();
    for extra in raw
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
    {
        if !apps.contains(&extra) {
            apps.push(extra);
        }
    }
    apps
}

/// Whether any of `names` (an application name, a binary path, an exe file name)
/// contains a listed voice-app fragment. Case-insensitive; the list is lowercase.
pub fn voice_app_matches<'a>(names: impl IntoIterator<Item = &'a str>, apps: &[String]) -> bool {
    names
        .into_iter()
        .map(str::to_ascii_lowercase)
        .any(|n| apps.iter().any(|a| n.contains(a.as_str())))
}

/// Operator and dispatch knobs resolved once. Session-mutated values stay at
/// their call sites. Unused-on-this-platform fields stay so `Debug` and the
/// parser remain one platform-neutral function.
#[derive(Debug, Clone, Default)]
pub struct HostConfig {
    /// Row `host_name` — Moonlight `<hostname>` and the mDNS instance name.
    /// Blank = the machine hostname. Display-only; the DNS `<label>.local.`
    /// is a sanitized label so a spacey name cannot produce an invalid record.
    pub host_name: Option<String>,
    /// Row `clipboard`.
    pub clipboard: ClipboardPolicy,
    /// `PUNKTFUNK_MGMT_BIND` — management listen (`IP:PORT`). `--mgmt-bind` wins.
    /// Unset = `0.0.0.0:47990` (Sunshine's web UI; the only port the two share).
    /// Lives in `host.env` because a package upgrade rewrites the unit file.
    /// Raw string: `main.rs` turns a bad value into the same error as the flag.
    pub mgmt_bind: Option<String>,
    /// `PUNKTFUNK_NATIVE_PORT` — native QUIC control port. `--native-port` wins.
    /// Unset = 9777. Raw string so a typo is a startup error, not a silent 9777.
    pub native_port: Option<String>,
    /// Row `gamestream` — GameStream/Moonlight-compat planes. **Default OFF**: they
    /// carry plain-HTTP pairing.
    pub gamestream: bool,
    /// Row `webtransport` — the browser plane. **Default OFF**, like GameStream: a new
    /// externally-reachable transport should not appear on a host because it was upgraded.
    pub webtransport: bool,
    /// `PUNKTFUNK_WEBTRANSPORT_ORIGINS` — comma-separated browser origins allowed to open a
    /// session (`https://host:47990`). Unset = any, because a host has no way to know its own
    /// origin until the console offers the choice. WebTransport is not subject to CORS, so this
    /// is the only thing that separates the real client from any page the user has open.
    pub webtransport_origins: Vec<String>,
    /// `PUNKTFUNK_WEBTRANSPORT_BIND` — browser-plane listen address. `--webtransport-bind` wins.
    /// Unset = `[::]`, every interface, matching the native plane. Narrow it to put the browser
    /// plane on one interface without moving the rest of the host.
    pub webtransport_bind: Option<String>,
    /// `PUNKTFUNK_WEBTRANSPORT_PORT` — browser-plane listen port. `--webtransport-port` wins.
    /// Unset = 9778. Raw string so a typo is a startup error, not a silent default.
    pub webtransport_port: Option<String>,
    /// `PUNKTFUNK_ENCODER` — encoder-backend override (lowercased). Empty = auto-detect by GPU vendor.
    pub encoder_pref: String,
    /// `PUNKTFUNK_RENDER_ADAPTER` — discrete render-GPU pin by description substring.
    /// `Some` even when empty: empty still counts as set for presence checks.
    pub render_adapter: Option<String>,
    /// `PUNKTFUNK_ZEROCOPY` — Windows D3D11 zero-copy encode input. `None` defers to
    /// the per-vendor default (AMF on, QSV off).
    pub zerocopy: Option<bool>,
    /// Row `ten_bit` — host policy gate for HEVC Main10 / AV1. **Default ON**. The
    /// host only *allows* 10-bit; the session still needs `VIDEO_CAP_10BIT` and
    /// `can_encode_10bit`. Independent of `four_four_four`.
    pub ten_bit: bool,
    /// Row `chroma_444` — host policy gate for HEVC 4:4:4. **Default ON**. The host
    /// only *allows* 4:4:4; the session still needs the client to advertise it, HEVC,
    /// full-chroma capture, and the encode probe. Independent of `ten_bit`.
    pub four_four_four: bool,
    /// `PUNKTFUNK_CHACHA20` — host policy gate for ChaCha20-Poly1305
    /// (`design/chacha20-session-cipher.md`). **Default ON**, explicit-off.
    /// The host only *allows* it; a session uses ChaCha only when the client
    /// advertised `VIDEO_CAP_CHACHA20`. Everyone else stays AES-128-GCM.
    pub chacha20: bool,
    /// Row `audio_output_mode` — see [`AudioOutputMode`].
    pub audio_output_mode: AudioOutputMode,
    /// Row `audio_voice_chat` — see [`VoiceChatRoute`].
    pub audio_voice_chat: VoiceChatRoute,
    /// Row `audio_voice_apps` — see [`parse_voice_apps`]. Never empty.
    pub audio_voice_apps: Vec<String>,
    /// `PUNKTFUNK_AUDIO_QUALITY` — encode tier (`low`/`standard`/`high`; default
    /// `high`). Raw string: the table lives in `punktfunk-core`. The audio thread
    /// warns on an unknown spelling rather than silently downgrading.
    pub audio_quality: Option<String>,
    /// `PUNKTFUNK_AUDIO_REDUNDANCY` — force the redundant `0xD2` audio plane.
    /// `None` = automatic: only to a client that asked, and only while losing packets.
    pub audio_redundancy: Option<bool>,
    /// `PUNKTFUNK_AUDIO_HIRES` — host policy gate for lossless `0xD3`
    /// (`design/hi-res-audio.md`). **Default ON**, explicit-off. The host only
    /// *allows* the plane; the client's format pick is the session switch.
    /// [`env_on`] treats a client-shaped `96000/24` as allow. `0` forces Opus.
    pub audio_hires: bool,
    /// `PUNKTFUNK_PERF` — per-stage timing instrumentation.
    pub perf: bool,
    /// `PUNKTFUNK_VIDEO_SOURCE` — `virtual` (default: per-client virtual output) /
    /// `portal` (an existing monitor); anything else, including `synthetic`, is
    /// the test pattern.
    pub video_source: Option<String>,
    /// `PUNKTFUNK_CAPTURE_MONITOR` — pin capture at a named physical monitor
    /// (`DP-1`). Config, not a prompt: a `--user` service has nobody to answer a
    /// chooser. A name that matches no head is a hard error. Linux-only;
    /// `design/per-monitor-portal-capture.md`.
    pub capture_monitor: Option<String>,
    /// `PUNKTFUNK_PORTAL_CURSOR_MODE` — `auto` (default) · `hidden` · `embedded` ·
    /// `metadata`. Preference, not a command: `portal_cursor::pick` closes the
    /// session if the backend does not advertise it. `embedded` is the safe pin.
    pub portal_cursor_mode: Option<String>,
    /// `PUNKTFUNK_COMPOSITOR` — explicit compositor override (operator/CI/test).
    /// Not the runtime-detected session; `apply_session_env` never writes this.
    pub compositor: Option<String>,
    /// `PUNKTFUNK_GAMEPAD` — virtual-pad backend preference, fed to `pick_gamepad`.
    pub gamepad: Option<String>,
    /// `PUNKTFUNK_GAMESCOPE_STEAM` — force `--steam` on every bare headless gamescope
    /// launch. Steam titles already pass it; this is for non-Steam. Managed
    /// gamescope-session-plus/SteamOS sessions ignore it.
    pub gamescope_steam: bool,
    /// `PUNKTFUNK_GAMESCOPE_GRAB_CURSOR` — `--force-grab-cursor` on a real game
    /// launch. Default OFF: relative mode breaks absolute-pointer titles and menus.
    pub gamescope_grab_cursor: bool,
    /// `PUNKTFUNK_GAMESCOPE_SPLASH` — splash on every bare headless gamescope spawn.
    /// gamescope only composites (and pushes PipeWire) when a client paints.
    /// **Default ON**; `=0` is the escape hatch.
    pub gamescope_splash: bool,
    /// `PUNKTFUNK_GAMESCOPE_ISOLATE` — per-session EIS/audio/mic planes
    /// (`design/gamescope-multiuser.md`). **Default ON**; `=0` restores shared
    /// host-lifetime planes. Shared-desktop and managed/attach routes are untouched.
    pub gamescope_isolate: bool,
    /// `PUNKTFUNK_STEAM_SEAT_HOME` — run a dedicated Steam launch under the
    /// session's own `HOME` (`pf_paths::seat_home`), so it neither waits for the
    /// desktop Steam to shut down nor shares its account
    /// (`design/steam-seats-warm-launch-implementation-plan.md`). **Default OFF.**
    /// Needs a native Steam to clone; a seat signs in on its own.
    pub steam_seat_home: bool,
    /// `PUNKTFUNK_STEAM_SEAT_SANDBOX` — show a seat's nested Steam only the pads
    /// this session created, by running it under `bwrap` with a per-seat
    /// `/dev/input` and `/dev/hidraw*`
    /// (`design/steam-seats-warm-launch-implementation-plan.md` WP-S3).
    /// **Default OFF.** Needs a seat home and `bwrap` on `PATH`.
    pub steam_seat_sandbox: bool,
    /// `PUNKTFUNK_STEAM_PREWARM` — how many seats the host may hold a Big Picture Steam up for
    /// before their clients connect, so a launch skips Steam's 13–30 s cold boot
    /// (`design/steam-seats-warm-launch-implementation-plan.md` WP-S2). **Default 1**; `0` is
    /// off. Each parked seat costs about a gigabyte, and only a seat home can be pre-warmed.
    pub steam_prewarm: u32,
    /// `PUNKTFUNK_GAMESCOPE_HDR` — allow HDR on gamescope. The host probes the
    /// punktfunk build (`packaging/gamescope`) and stays SDR if missing; this only
    /// decides whether HDR is *attempted*. **Default ON**, matching `PUNKTFUNK_10BIT`.
    pub gamescope_hdr: bool,
    /// `PUNKTFUNK_GAMESCOPE_SDR_NITS` — starting SDR luminance inside the PQ container
    /// (`--hdr-sdr-content-nits`). `None` = 203 nits (BT.2408), what our clients
    /// decode against. See `SDR_REFERENCE_WHITE_NITS`.
    pub gamescope_sdr_nits: Option<u32>,
    /// `PUNKTFUNK_GAMESCOPE_BIND` — bind patched gamescope over `/usr/bin/gamescope`
    /// in the session unit's mount namespace. `gamescope-session-plus` hardcodes
    /// that path (`pf-vdisplay`'s `gamescope.rs`).
    ///
    /// Three-valued. A user-unit mount namespace maps only this uid, so
    /// root-owned `/tmp/.X11-unix` reads as `nobody` and Xwayland refuses to start.
    /// `None` = AUTO (arm only when the script cannot reach gamescope another way).
    /// `Some(false)` = never. `Some(true)` = force; a failed redirect still disarms.
    pub gamescope_bind: Option<bool>,
    /// `PUNKTFUNK_GAMESCOPE_REFRESH_RATES` — extra Hz (comma-separated) a gamescope
    /// session offers on top of the rate it runs at. Can only ADD; the session rate
    /// is always included. Empty = negotiated rate only. Ignored on stock gamescope.
    pub gamescope_refresh_rates: Vec<u32>,
    /// `PUNKTFUNK_RECOVER_SESSION_CMD` — operator hook (debounced) when a client
    /// connects with no graphical session for this uid. Unset/empty = disabled.
    pub recover_session_cmd: Option<String>,
    /// `PUNKTFUNK_ON_CONNECT_CMD` — `client.connected` hook: detached, event JSON
    /// on stdin + `PF_EVENT_*`. Filters live in `hooks.json`. Unset/empty = disabled.
    pub on_connect_cmd: Option<String>,
    /// `PUNKTFUNK_ON_DISCONNECT_CMD` — `client.disconnected` sibling of
    /// [`Self::on_connect_cmd`].
    pub on_disconnect_cmd: Option<String>,
    /// Row `max_fps` — game-side frame limiter. `None` (`0`) = no limit. Caps
    /// compositor render rate, not the session: a 120 Hz session over a 60 fps cap
    /// still sends 120 frames (60 repeats). gamescope: `--nested-refresh`, 1..=240.
    pub max_fps: Option<u32>,
    /// Row `pyrowave_bpp` — bits per pixel a PyroWave frame gets at 4:2:0 SDR.
    pub pyrowave_bpp: f64,
    /// `PUNKTFUNK_VDISPLAY_HZ_MULT` — virtual-display refresh as a multiple of the
    /// session rate; the stream stays at the session rate. Default 1; 2 halves
    /// worst-case age (~16 ms at 60 Hz) without extra wire frames. Clamped 1..=4.
    pub vdisplay_hz_mult: u32,
    /// `PUNKTFUNK_GAMESCOPE_VRR=0` — opt out of adaptive sync. Default on: capable
    /// gamescope gets `--adaptive-sync` + `--framerate-limit` at the game rate so
    /// it paints on the game's commit. Inert on stock gamescope (`adaptive_sync_args`).
    pub gamescope_vrr: bool,
}

impl HostConfig {
    /// Fields parsed from their env spelling. Each read goes through the rows, so a field whose
    /// env name has a registry row also sees the console's value. The typed rows are
    /// [`Self::apply_settings`].
    fn from_rows(rows: &[Resolved]) -> Self {
        let val = |k: &str| store::knob_in(rows, k);
        // Presence, not value.
        let flag = |k: &str| val(k).is_some();
        let on = |k: &str| val(k).map(|s| is_on(&s));
        Self {
            // Blank-is-unset: `PUNKTFUNK_MGMT_BIND=` means default.
            mgmt_bind: val("PUNKTFUNK_MGMT_BIND")
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            native_port: val("PUNKTFUNK_NATIVE_PORT")
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            webtransport_origins: val("PUNKTFUNK_WEBTRANSPORT_ORIGINS")
                .unwrap_or_default()
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
            webtransport_bind: val("PUNKTFUNK_WEBTRANSPORT_BIND")
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            webtransport_port: val("PUNKTFUNK_WEBTRANSPORT_PORT")
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            encoder_pref: encoder_pref(val("PUNKTFUNK_ENCODER")),
            render_adapter: val("PUNKTFUNK_RENDER_ADAPTER"),
            zerocopy: on("PUNKTFUNK_ZEROCOPY"),
            chacha20: on("PUNKTFUNK_CHACHA20").unwrap_or(true),
            audio_quality: val("PUNKTFUNK_AUDIO_QUALITY").map(|s| s.trim().to_lowercase()),
            audio_redundancy: on("PUNKTFUNK_AUDIO_REDUNDANCY"),
            audio_hires: on("PUNKTFUNK_AUDIO_HIRES").unwrap_or(true),
            perf: flag("PUNKTFUNK_PERF"),
            // Defaults to `virtual` — the flagship per-client virtual output. It used to be unset,
            // which fell through to the synthetic test pattern: fine for a dev box that always has
            // a host.env, wrong for a packaged install, whose unit no longer requires that file at
            // all. `synthetic` is still reachable by naming it (any unrecognised value lands there).
            video_source: val("PUNKTFUNK_VIDEO_SOURCE").or_else(|| Some("virtual".to_string())),
            // Emptied-to-None: `PUNKTFUNK_CAPTURE_MONITOR=` is unset, not a blank name.
            capture_monitor: val("PUNKTFUNK_CAPTURE_MONITOR")
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            // Emptied-to-None. Spellings are parsed at `portal_cursor::want`.
            portal_cursor_mode: val("PUNKTFUNK_PORTAL_CURSOR_MODE")
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            compositor: val("PUNKTFUNK_COMPOSITOR"),
            gamepad: val("PUNKTFUNK_GAMEPAD"),
            gamescope_steam: val("PUNKTFUNK_GAMESCOPE_STEAM").is_some_and(|s| {
                matches!(
                    s.trim().to_ascii_lowercase().as_str(),
                    "1" | "true" | "yes" | "on"
                )
            }),
            gamescope_grab_cursor: val("PUNKTFUNK_GAMESCOPE_GRAB_CURSOR").is_some_and(|s| {
                matches!(
                    s.trim().to_ascii_lowercase().as_str(),
                    "1" | "true" | "yes" | "on"
                )
            }),
            gamescope_splash: on("PUNKTFUNK_GAMESCOPE_SPLASH").unwrap_or(true),
            gamescope_isolate: on("PUNKTFUNK_GAMESCOPE_ISOLATE").unwrap_or(true),
            steam_seat_home: on("PUNKTFUNK_STEAM_SEAT_HOME").unwrap_or(false),
            steam_seat_sandbox: on("PUNKTFUNK_STEAM_SEAT_SANDBOX").unwrap_or(false),
            steam_prewarm: val("PUNKTFUNK_STEAM_PREWARM")
                .and_then(|s| s.trim().parse::<u32>().ok())
                .unwrap_or(1)
                .min(8),
            gamescope_hdr: on("PUNKTFUNK_GAMESCOPE_HDR").unwrap_or(true),
            gamescope_sdr_nits: val("PUNKTFUNK_GAMESCOPE_SDR_NITS")
                .and_then(|s| s.trim().parse::<u32>().ok())
                .filter(|n| (1..=10_000).contains(n)),
            // Unset is AUTO; `=0` is stock gamescope; `=1` is force.
            gamescope_bind: on("PUNKTFUNK_GAMESCOPE_BIND"),
            // Junk entries are dropped; this only widens a menu.
            gamescope_refresh_rates: parse_refresh_rates(
                val("PUNKTFUNK_GAMESCOPE_REFRESH_RATES").as_deref(),
            ),
            recover_session_cmd: val("PUNKTFUNK_RECOVER_SESSION_CMD")
                .filter(|s| !s.trim().is_empty()),
            on_connect_cmd: val("PUNKTFUNK_ON_CONNECT_CMD").filter(|s| !s.trim().is_empty()),
            on_disconnect_cmd: val("PUNKTFUNK_ON_DISCONNECT_CMD").filter(|s| !s.trim().is_empty()),
            vdisplay_hz_mult: val("PUNKTFUNK_VDISPLAY_HZ_MULT")
                .and_then(|s| s.trim().parse::<u32>().ok())
                .unwrap_or(1)
                .clamp(1, 4),
            gamescope_vrr: val("PUNKTFUNK_GAMESCOPE_VRR").as_deref().map(str::trim) != Some("0"),
            ..Self::default()
        }
    }

    /// Registry fields from resolved rows. Values are already validated, so a type
    /// mismatch here is a registry bug and falls back to the field default.
    fn apply_settings(&mut self, rows: &[Resolved]) {
        let get = |id: &str| {
            rows.iter()
                .find(|r| r.setting.id == id)
                .map(|r| &r.value)
                .unwrap_or(&serde_json::Value::Null)
        };
        let text = |id: &str| get(id).as_str().unwrap_or_default().to_string();
        let on = |id: &str| get(id).as_bool().unwrap_or_default();
        self.gamestream = on("gamestream");
        self.webtransport = on("webtransport");
        self.clipboard = ClipboardPolicy::parse(&text("clipboard")).unwrap_or_default();
        self.host_name = Some(text("host_name")).filter(|s| !s.is_empty());
        self.ten_bit = on("ten_bit");
        self.four_four_four = on("chroma_444");
        // 0 means no limit, not "stream nothing".
        self.max_fps = get("max_fps")
            .as_u64()
            .filter(|&f| f > 0)
            .map(|f| f.min(240) as u32);
        self.pyrowave_bpp = get("pyrowave_bpp")
            .as_f64()
            .unwrap_or(registry::PYROWAVE_BPP);
        self.audio_output_mode =
            AudioOutputMode::parse(&text("audio_output_mode")).unwrap_or_default();
        self.audio_voice_chat =
            VoiceChatRoute::parse(&text("audio_voice_chat")).unwrap_or_default();
        let apps: Vec<&str> = get("audio_voice_apps")
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default();
        self.audio_voice_apps = parse_voice_apps(Some(&apps.join(",")));
    }
}

/// `"60, 90,120"` → `[60, 90, 120]`, sorted and deduped. Junk and out-of-range
/// rates are skipped rather than rejecting the list.
fn parse_refresh_rates(raw: Option<&str>) -> Vec<u32> {
    let mut out: Vec<u32> = raw
        .unwrap_or_default()
        .split(',')
        .filter_map(|s| s.trim().parse::<u32>().ok())
        .filter(|&hz| (1..=1000).contains(&hz))
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

impl HostConfig {
    /// Compositor refresh for the GAME: the session rate, capped by [`Self::max_fps`].
    /// Session mode, encoder, and wire never go through here.
    ///
    /// `0` in means `0` out. A zero rate is rejected upstream.
    pub fn game_fps(&self, session_hz: u32) -> u32 {
        match self.max_fps {
            Some(cap) if session_hz > cap => cap,
            _ => session_hz,
        }
    }
}

/// `PUNKTFUNK_ENCODER`, lower-cased. On Windows a software pin becomes `auto`: the driver
/// encodes, so a session opened on it would pass the handshake and die at the encoder open.
fn encoder_pref(raw: Option<String>) -> String {
    let pref = raw.unwrap_or_default().to_ascii_lowercase();
    if cfg!(windows) && matches!(pref.as_str(), "sw" | "software" | "openh264") {
        eprintln!(
            "punktfunk: PUNKTFUNK_ENCODER={pref:?} — Windows has no software encoder since the \
             driver took over encoding; using auto"
        );
        return "auto".into();
    }
    pref
}

/// The current host configuration. Built on first access; a settings write replaces it.
pub fn config() -> &'static HostConfig {
    &snapshot().config
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(max_fps: Option<u32>) -> HostConfig {
        HostConfig {
            max_fps,
            ..Default::default()
        }
    }

    #[test]
    fn game_fps_caps_only_above_the_limit() {
        for hz in [24, 30, 60, 120, 144, 240] {
            assert_eq!(cfg(None).game_fps(hz), hz);
        }
        // Ceiling, not a target: a session below the limit keeps its own rate.
        let c = cfg(Some(60));
        assert_eq!(c.game_fps(120), 60);
        assert_eq!(c.game_fps(60), 60);
        assert_eq!(c.game_fps(30), 30);
        // An invalid rate stays invalid rather than being laundered into a real one.
        assert_eq!(c.game_fps(0), 0);
    }

    #[test]
    fn refresh_rate_list_parses_and_tolerates_junk() {
        assert_eq!(parse_refresh_rates(Some("60,90,120")), vec![60, 90, 120]);
        assert_eq!(
            parse_refresh_rates(Some(" 120, 60 ,90, 60")),
            vec![60, 90, 120]
        );
        assert!(parse_refresh_rates(None).is_empty());
        assert!(parse_refresh_rates(Some("")).is_empty());
        assert!(parse_refresh_rates(Some("   ")).is_empty());
        // A typo costs its own entry, never the whole list.
        assert_eq!(parse_refresh_rates(Some("60,abc,120")), vec![60, 120]);
        // 0 is not a refresh rate; 1920 is a width.
        assert_eq!(parse_refresh_rates(Some("0,60,1920")), vec![60]);
    }

    #[test]
    fn audio_output_mode_parses_its_spellings() {
        for (s, want) in [
            ("client_only", AudioOutputMode::ClientOnly),
            ("client-only", AudioOutputMode::ClientOnly),
            ("  CLIENT  ", AudioOutputMode::ClientOnly),
            ("host_and_client", AudioOutputMode::HostAndClient),
            ("both", AudioOutputMode::HostAndClient),
            ("follow_default", AudioOutputMode::FollowDefault),
            ("follow", AudioOutputMode::FollowDefault),
        ] {
            assert_eq!(AudioOutputMode::parse(s), Some(want), "{s:?}");
        }
        // Unknown spellings are rejected, not silently re-routed.
        for s in ["", "silent", "off", "true"] {
            assert_eq!(AudioOutputMode::parse(s), None, "{s:?}");
        }
        for m in [
            AudioOutputMode::ClientOnly,
            AudioOutputMode::HostAndClient,
            AudioOutputMode::FollowDefault,
        ] {
            assert_eq!(AudioOutputMode::parse(m.as_str()), Some(m));
        }
    }

    #[test]
    fn voice_chat_route_and_app_list_parse() {
        assert_eq!(VoiceChatRoute::parse(" Host "), Some(VoiceChatRoute::Host));
        assert_eq!(
            VoiceChatRoute::parse("speakers"),
            Some(VoiceChatRoute::Host)
        );
        assert_eq!(
            VoiceChatRoute::parse("stream"),
            Some(VoiceChatRoute::Stream)
        );
        assert_eq!(VoiceChatRoute::parse("both"), None);
        assert_eq!(VoiceChatRoute::default(), VoiceChatRoute::Stream);
        // A blank or absent list is the default; a typed one adds to it, lowercased, no repeats.
        assert_eq!(parse_voice_apps(None), DEFAULT_VOICE_APPS);
        assert_eq!(parse_voice_apps(Some(" , ")), DEFAULT_VOICE_APPS);
        let extended = parse_voice_apps(Some("Discord, firefox ,,"));
        assert_eq!(extended.len(), DEFAULT_VOICE_APPS.len() + 1);
        assert_eq!(extended.last().map(String::as_str), Some("firefox"));
        assert!(voice_app_matches(["Discord"], &extended));
        assert!(voice_app_matches(
            ["WEBRTC VoiceEngine", "DiscordCanary.exe"],
            &extended
        ));
        assert!(voice_app_matches(["/usr/bin/vesktop"], &extended));
        assert!(voice_app_matches(["Firefox"], &extended));
        assert!(!voice_app_matches(
            ["Firefox", "firefox.exe"],
            &parse_voice_apps(None)
        ));
        assert!(!voice_app_matches(std::iter::empty(), &extended));
    }

    /// `prefers_host_hardware` and `keeps_default` must stay mutually exclusive:
    /// conflating them would either silence the host or stomp the operator's devices.
    #[test]
    fn audio_output_mode_predicates_are_disjoint() {
        assert_eq!(AudioOutputMode::default(), AudioOutputMode::ClientOnly);
        for m in [
            AudioOutputMode::ClientOnly,
            AudioOutputMode::HostAndClient,
            AudioOutputMode::FollowDefault,
        ] {
            assert!(!(m.prefers_host_hardware() && m.keeps_default()), "{m:?}");
        }
        assert!(AudioOutputMode::HostAndClient.prefers_host_hardware());
        assert!(AudioOutputMode::FollowDefault.keeps_default());
        assert!(!AudioOutputMode::ClientOnly.prefers_host_hardware());
        assert!(!AudioOutputMode::ClientOnly.keeps_default());
    }
}
