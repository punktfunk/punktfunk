//! `punktfunk-session` — the Vulkan session binary (punktfunk-planning
//! `linux-client-rearchitecture.md`, Phase 1: the software-path presenter MVP, which IS
//! the power-user CLI build).
//!
//! One stream session per invocation: `--connect host[:port]` (+ `--fp HEX`,
//! `--launch id`, `--fullscreen`), exits when the session ends. Reads the same identity
//! / known-hosts / settings stores as the desktop shell on each OS — the GTK client
//! (`punktfunk-client`) on Linux, the WinUI client on Windows — so pairing on either side
//! makes the other connect silently. `--pair - --connect host` runs the ceremony here,
//! with no window and no toolkit, for machines that have only a shell.
//!
//! Stdout is the machine interface (the shell↔session contract): `{"ready":true}` after
//! the first presented frame, `stats:` lines per 1 s window, one `{"error": …}` /
//! `{"ended": …}` JSON line on the way out. Logs go to stderr. Exit codes: 0 clean end,
//! 2 connect failed, 3 trust rejected / pairing required, 4 presenter init failed.
// `deny`, not `forbid`: edition 2024 makes this bin's startup env writes unsafe, and they
// sit under localized `#[allow(unsafe_code)]` with SAFETY comments. `forbid` cannot be
// overridden there and refuses the file. Do not name the env APIs here — the unsafe-hygiene
// gate counts mentions against this file's baseline.
#![deny(unsafe_code)]

#[cfg(all(any(target_os = "linux", windows), feature = "ui"))]
mod console;

/// The session control socket: a line-per-connection unix socket other same-user
/// processes use to poke the RUNNING stream — today two verbs, `guide` and `qam`, which
/// press the HOST's system buttons (the Decky panel's "Steam menu / Quick access on the
/// host" buttons; see `GamepadService::tap_guide`). Plain text, no JSON: `<verb>\n` in,
/// `ok\n` / `err\n` back.
///
/// The path is `$XDG_RUNTIME_DIR/punktfunk-session-ctl.sock` — inside the flatpak app
/// runtime dir (`…/app/$FLATPAK_ID/`) when sandboxed, the ONE runtime path a flatpak and
/// the host see identically, which is what lets the Decky backend (outside the sandbox)
/// reach a flatpak-run session.
#[cfg(all(unix, any(target_os = "linux", windows)))]
mod ctl_socket {
    use pf_client_core::gamepad::GamepadService;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::PathBuf;

    fn path() -> Option<PathBuf> {
        let mut p = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR")?);
        if let Ok(id) = std::env::var("FLATPAK_ID") {
            p.push("app");
            p.push(id);
        }
        Some(p.join("punktfunk-session-ctl.sock"))
    }

    /// Bind + serve on a background thread, once per process (later calls no-op). Any
    /// failure just logs at debug — the socket is a convenience surface, never worth
    /// failing a stream over.
    pub(crate) fn spawn(gamepad: GamepadService) {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(move || {
            let Some(path) = path() else { return };
            // Bind FIRST. Unlinking on sight handed the socket to whichever session started
            // last, leaving the running one holding an unreachable inode — so the Decky
            // panel's guide/qam reached only the newest stream.
            let listener = match UnixListener::bind(&path) {
                Ok(l) => l,
                Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
                    // Held, or left behind by a session that died? A live socket accepts;
                    // only a stale one is ours to replace.
                    if UnixStream::connect(&path).is_ok() {
                        tracing::debug!(path = %path.display(), "session ctl socket held by another session");
                        return;
                    }
                    let _ = std::fs::remove_file(&path);
                    match UnixListener::bind(&path) {
                        Ok(l) => l,
                        Err(e) => {
                            tracing::debug!(error = %e, path = %path.display(), "session ctl socket unavailable");
                            return;
                        }
                    }
                }
                Err(e) => {
                    tracing::debug!(error = %e, path = %path.display(), "session ctl socket unavailable");
                    return;
                }
            };
            let spawned = std::thread::Builder::new()
                .name("pf-session-ctl".into())
                .spawn(move || {
                    for stream in listener.incoming() {
                        let Ok(mut s) = stream else { continue };
                        // A peer that connects and says nothing must not hold the one thread
                        // that serves this socket.
                        let _ = s.set_read_timeout(Some(std::time::Duration::from_secs(5)));
                        let mut line = String::new();
                        if BufReader::new(&s).read_line(&mut line).is_err() {
                            continue;
                        }
                        let ok = match line.trim() {
                            "guide" => {
                                gamepad.tap_guide();
                                true
                            }
                            "qam" => {
                                gamepad.tap_qam();
                                true
                            }
                            _ => false,
                        };
                        let _ = s.write_all(if ok { b"ok\n" } else { b"err\n" });
                    }
                });
            if let Err(e) = spawned {
                tracing::debug!(error = %e, "spawning the session ctl thread");
            }
        });
    }
}

#[cfg(any(target_os = "linux", windows, test))]
fn resolve_hdr_enabled(
    setting: bool,
    output_hdr: bool,
    presentable: impl FnOnce() -> bool,
) -> bool {
    setting && output_hdr && presentable()
}

#[cfg(any(target_os = "linux", windows))]
mod session_main {
    use pf_client_core::gamepad::GamepadService;
    use pf_client_core::orchestrate::ResolvedSpec;
    use pf_client_core::session::{Dial, Probes, SessionParams};
    use pf_client_core::trust;
    use punktfunk_core::config::{GamepadPref, Mode};
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    use std::time::Duration;

    pub const EXIT_CONNECT_FAILED: u8 = 2;
    pub const EXIT_TRUST_REJECTED: u8 = 3;
    pub const EXIT_PRESENTER_FAILED: u8 = 4;

    /// The value following `flag` in argv, if present (`--flag value`).
    pub(crate) fn arg_value(flag: &str) -> Option<String> {
        std::env::args()
            .skip_while(|a| a != flag)
            .nth(1)
            .filter(|v| !v.starts_with("--"))
    }

    pub(crate) fn arg_flag(flag: &str) -> bool {
        std::env::args().any(|a| a == flag)
    }

    /// The stats-overlay tier a session starts on: the resolved setting, except that
    /// `--stats` (tooling/debug runs) forces the overlay VISIBLE without demoting an
    /// explicitly chosen richer tier.
    ///
    /// One helper because three callers need the identical rule — both run modes' presenter
    /// options and the per-launch [`session_params`] — and a fourth reading of it would be
    /// the bug this is here to prevent.
    pub(crate) fn stats_tier(settings: &trust::Settings) -> trust::StatsVerbosity {
        stats_tier_with(settings.stats_verbosity(), arg_flag("--stats"))
    }

    /// [`stats_tier`]'s rule, with argv lifted out so it is testable.
    pub(crate) fn stats_tier_with(
        chosen: trust::StatsVerbosity,
        stats_flag: bool,
    ) -> trust::StatsVerbosity {
        match chosen {
            trust::StatsVerbosity::Off if stats_flag => trust::StatsVerbosity::Normal,
            v => v,
        }
    }

    /// Running under Gaming Mode (a Deck, or any gamescope session): the environment
    /// where the local Steam UI owns the physical Steam/QAM buttons — the system-button
    /// "auto" policy keys off this. Gaming Mode means gamescope is really there, which is
    /// not what the bare env vars say — see [`pf_client_core::gamescope`].
    pub(crate) fn gaming_mode() -> bool {
        pf_client_core::gamescope::under_gamescope()
    }

    /// Run fullscreen: `--fullscreen`, or the Deck/gamescope env as a fallback so a
    /// manual launch under Gaming Mode does the right thing too. (Browse-mode only —
    /// gated with `mod browse`, its one caller.)
    #[cfg(feature = "ui")]
    pub(crate) fn fullscreen_mode() -> bool {
        arg_flag("--fullscreen") || gaming_mode()
    }

    /// `--window-pos X,Y` → the window's top-left in desktop coordinates (a spawning
    /// shell passes its own position so the session opens on the same monitor); absent or
    /// unparsable = centered on the primary display.
    pub(crate) fn window_pos() -> Option<(i32, i32)> {
        let v = arg_value("--window-pos")?;
        let (x, y) = v.split_once(',')?;
        Some((x.trim().parse().ok()?, y.trim().parse().ok()?))
    }

    /// `--pair - --connect host[:port]` — the SPAKE2 PIN ceremony with no window, no GTK
    /// and no console UI, so a machine that has only SSH can be enrolled: an embedded/kiosk
    /// client, a headless box, an image being provisioned. Writes the verified host into the
    /// same known-hosts store `--connect` reads, so pairing here is exactly what makes the
    /// later stream connect silently.
    ///
    /// Deliberately identical in shape and output to `punktfunk-client --pair` (which stays
    /// the desktop route) — the difference is only that this binary carries no toolkit, so it
    /// is the one a minimal image installs. Present in the `--no-default-features` build too:
    /// enrolment must not be the reason an embedded image has to pull in Skia.
    fn headless_pair(pin: &str) -> u8 {
        let Some(target) = arg_value("--connect") else {
            eprintln!("--pair requires --connect host[:port]");
            return EXIT_CONNECT_FAILED;
        };
        let (addr, port) = parse_host_port(&target);
        // The label the HOST files this client under. A headless box has nobody to ask, so
        // the hostname is the only name that will mean anything in the paired-devices list.
        let name = arg_value("--name").unwrap_or_else(trust::device_name);

        let identity = match trust::load_or_create_identity() {
            Ok(i) => i,
            Err(e) => {
                eprintln!("client identity: {e:#}");
                return EXIT_CONNECT_FAILED;
            }
        };
        match trust::pair_with_host(&addr, port, &identity, pin, &name) {
            Ok(fp) => {
                let fp_hex = trust::hex(&fp);
                if let Err(e) = trust::persist_host(
                    &arg_value("--host-label").unwrap_or_else(|| addr.clone()),
                    &addr,
                    port,
                    &fp_hex,
                    true,
                ) {
                    eprintln!("couldn't save the host: {e:#}");
                }
                trust::forget_placeholder(&addr, port);
                println!("paired {addr}:{port} fp={fp_hex}");
                0
            }
            Err(e) => {
                eprintln!("{}", trust::pair_error_message(&e));
                EXIT_TRUST_REJECTED
            }
        }
    }

    /// `host[:port]`, port defaulting to the native 9777. Shared parser: a plain
    /// `rsplit_once(':')` reads the bare IPv6 `::1` as host `:` port `1`, which then both
    /// dials the wrong address and misses the saved record keyed by the right one.
    pub(crate) fn parse_host_port(target: &str) -> (String, u16) {
        pf_client_core::deeplink::parse_addr_port(target).unwrap_or_else(|| {
            eprintln!("unparsable port in '{target}', using default 9777");
            (target.to_string(), pf_client_core::deeplink::DEFAULT_PORT)
        })
    }

    /// `--preset <id|name>` — the preset this one session runs with, overriding the host's own
    /// binding for this launch only (never rebinding it): the shells' "Connect with ▸ X" and a
    /// `punktfunk://…&preset=` link both land here. Absent = honor the host's binding;
    /// `--preset ""` (or a bare `--preset`) forces the global defaults, which is how "Connect
    /// with ▸ Default settings" reaches a bound host. `--profile` is the pre-rename spelling.
    fn preset_arg() -> Option<String> {
        ["--preset", "--profile"]
            .into_iter()
            .find(|flag| arg_flag(flag))
            .map(|flag| arg_value(flag).unwrap_or_default())
    }

    /// Handshake budget. `--connect-timeout SECS` overrides the default.
    /// Request-access passes a longer budget: the host parks until Approve.
    pub(crate) fn connect_timeout() -> Duration {
        Duration::from_secs(
            arg_value("--connect-timeout")
                .and_then(|v| v.parse().ok())
                .unwrap_or(pf_client_core::orchestrate::DEFAULT_CONNECT_TIMEOUT_SECS),
        )
    }

    /// Clipboard decision, then [`params_from_spec`].
    ///
    /// `clipboard_override` is the spawner's per-host decision. `None` reads the
    /// record this pin resolves to. `display_hdr` is the selected output's HDR
    /// volume on Windows.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn session_params(
        settings: &trust::Settings,
        preset: Option<String>,
        preset_id: Option<String>,
        clipboard_override: Option<bool>,
        addr: String,
        port: u16,
        pin: [u8; 32],
        identity: (String, String),
        launch: Option<String>,
        gamepad: &GamepadService,
        native: Mode,
        display_hdr: Option<punktfunk_core::quic::HdrMeta>,
        force_software: Arc<AtomicBool>,
        vulkan: Option<pf_client_core::video::VulkanDecodeDevice>,
    ) -> SessionParams {
        let clipboard = clipboard_override.unwrap_or_else(|| {
            trust::KnownHosts::load()
                .resolve(Some(&trust::hex(&pin)), &addr, port)
                .is_some_and(|h| h.clipboard_sync)
        });
        let spec = ResolvedSpec {
            settings: settings.clone(),
            clipboard,
            preset,
            preset_id,
        };
        params_from_spec(
            spec,
            addr,
            port,
            pin,
            identity,
            launch,
            gamepad,
            native,
            display_hdr,
            force_software,
            vulkan,
        )
    }

    /// Gamepad-service writes and display probes, then the shared params fill.
    ///
    /// HDR, 4:4:4, and the pad service stay here: the library has no window
    /// and no device. Mode fallback and video caps live in
    /// [`ResolvedSpec::session_params`].
    #[allow(clippy::too_many_arguments)]
    fn params_from_spec(
        spec: ResolvedSpec,
        addr: String,
        port: u16,
        pin: [u8; 32],
        identity: (String, String),
        launch: Option<String>,
        gamepad: &GamepadService,
        native: Mode,
        display_hdr: Option<punktfunk_core::quic::HdrMeta>,
        force_software: Arc<AtomicBool>,
        vulkan: Option<pf_client_core::video::VulkanDecodeDevice>,
    ) -> SessionParams {
        let settings = &spec.settings;
        // Unconditional on every launch. Browse mode reuses one service, so a
        // cleared pin or a stream with forwarding off must undo the previous
        // choice before attach.
        gamepad
            .set_pinned((!settings.forward_pad.is_empty()).then(|| settings.forward_pad.clone()));
        gamepad.set_forwarding(settings.gamepad_forwarding);
        let game_mode = gaming_mode();
        gamepad.set_system_buttons(
            settings.system_buttons_forward(game_mode),
            settings.guide_gesture_enabled(game_mode),
        );
        // First build binds it, for --connect and for console launches.
        #[cfg(unix)]
        crate::ctl_socket::spawn(gamepad.clone());
        gamepad.set_pad_audio_prefs(
            settings.pad_haptics,
            pf_client_core::pad_audio::speaker_active(&settings.pad_speaker),
        );
        // Short-circuit: Full chroma off must not build an HEVC decoder.
        // The probe constructs one to ask about 4:4:4 profiles.
        let hevc_444_hardware = settings.enable_444
            && pf_client_core::video::hevc_444_hardware_decodable(vulkan.as_ref());
        if settings.enable_444 && !hevc_444_hardware {
            tracing::warn!(
                "Full chroma (4:4:4) requested but this device has no 4:4:4 HEVC decode — \
                 HEVC sessions ask for 4:2:0 instead. PyroWave still asks for 4:4:4: it \
                 decodes full chroma on any GPU."
            );
        }
        // Windows: HDR only when the selected output is presenting HDR.
        // `PUNKTFUNK_CLIENT_PEAK_NITS` is the headless bypass.
        #[cfg(windows)]
        let display_hdr = punktfunk_core::client::display_hdr_env_override().or(display_hdr);
        #[cfg(windows)]
        let output_hdr = display_hdr.is_some();
        #[cfg(not(windows))]
        let output_hdr = true;
        let hdr_enabled = super::resolve_hdr_enabled(settings.hdr_enabled, output_hdr, || {
            pf_client_core::video::hdr_presentable(vulkan.as_ref())
        });
        if settings.hdr_enabled && !hdr_enabled {
            tracing::warn!(
                reason = "the selected output or presentation path does not support PQ",
                "HDR request declined"
            );
        }
        // The service hears the chosen kind; the Hello hears Auto resolved.
        // The host builds each pad from its arrival, not only this default.
        let chosen = GamepadPref::from_name(&settings.gamepad).unwrap_or(GamepadPref::Auto);
        gamepad.set_kind_override(chosen);
        let gamepad_pref = match chosen {
            GamepadPref::Auto => gamepad.auto_pref(),
            explicit => explicit,
        };
        spec.session_params(
            Dial {
                host: addr,
                port,
                pin,
                launch,
                connect_timeout: connect_timeout(),
            },
            Probes {
                mode: native,
                identity,
                vulkan,
                force_software,
                gamepad: gamepad_pref,
                hdr_enabled,
                display_hdr,
                hevc_444_hardware,
                stats_verbosity: stats_tier(settings),
                latch_grid: std::sync::Arc::new(pf_client_core::session::LatchGrid::default()),
            },
        )
    }

    /// The window's starting size under Match-window: the persisted last size, so the
    /// first connect's mode already matches the glass; `None` (policy off / never
    /// stored) = the 1280×720 default.
    pub(crate) fn window_size(settings: &trust::Settings) -> Option<(u32, u32)> {
        (settings.match_window && settings.last_window_w > 0 && settings.last_window_h > 0)
            .then_some((settings.last_window_w, settings.last_window_h))
    }

    /// The Match-window policy hook for the presenter loop
    /// (design/midstream-resolution-resize.md D1/D2): `Some(persist)` turns the
    /// debounced resize→`Reconfigure` machinery on; the callback stores each resize-end's
    /// logical window size (load-modify-save, like the console settings screen) so the
    /// next launch opens at it.
    /// The Match-window policy hook (design/midstream-resolution-resize.md D1/D2). The
    /// callback used to load-modify-save the shared settings file from inside the renderer —
    /// one of that file's five concurrent writers, for a value only the parent needs. It now
    /// REPORTS the size on stdout and the spawner persists it
    /// (design/client-architecture-split.md §5).
    ///
    /// `persist_locally` keeps a hand-run session remembering its own window: nobody is
    /// listening to stdout there, so the event alone would drop the value. A spawned session
    /// leaves the write to its parent, which is the whole point.
    pub(crate) fn match_window(
        settings: &trust::Settings,
        persist_locally: bool,
    ) -> Option<Box<dyn FnMut(u32, u32)>> {
        settings.match_window.then(|| {
            Box::new(move |w: u32, h: u32| {
                machine_line(&format!("{{\"window\":{{\"w\":{w},\"h\":{h}}}}}"));
                if persist_locally {
                    pf_client_core::orchestrate::persist_window_size(w, h);
                }
            }) as Box<dyn FnMut(u32, u32)>
        })
    }

    /// One JSON status line on stdout (the shell parses these; strings hand-escaped via
    /// the minimal rules a reason string can need). `pub(crate)`: browse mode emits its
    /// failure through the same contract when spawned with `--json-status`.
    pub(crate) fn json_line(key: &str, msg: &str, trust_rejected: Option<bool>) {
        let escaped: String = msg
            .chars()
            .flat_map(|c| match c {
                '"' => vec!['\\', '"'],
                '\\' => vec!['\\', '\\'],
                '\n' => vec!['\\', 'n'],
                c if (c as u32) < 0x20 => vec![' '],
                c => vec![c],
            })
            .collect();
        match trust_rejected {
            Some(t) => machine_line(&format!(
                "{{\"{key}\":\"{escaped}\",\"trust_rejected\":{t}}}"
            )),
            None => machine_line(&format!("{{\"{key}\":\"{escaped}\"}}")),
        }
    }

    /// Write one line of the shell contract. A dropped write is NOT fatal: `println!` panics
    /// on EPIPE, so a shell that exited mid-stream used to abort the stream the user is still
    /// watching. Status nobody is left to read costs nothing to lose.
    pub(crate) fn machine_line(line: &str) {
        use std::io::Write as _;
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "{line}");
        let _ = out.flush();
    }

    /// The PipeWire endpoints the settings pickers offer, as
    /// `sink|source<TAB>node.name<TAB>description` lines — a debug window into the same
    /// enumeration the GTK shell probes.
    #[cfg(target_os = "linux")]
    fn list_audio() -> u8 {
        match pf_client_core::audio::devices() {
            Ok((sinks, sources)) => {
                for d in sinks {
                    println!("sink\t{}\t{}", d.name, d.description);
                }
                for d in sources {
                    println!("source\t{}\t{}", d.name, d.description);
                }
                0
            }
            Err(e) => {
                eprintln!("list-audio: {e:#}");
                EXIT_PRESENTER_FAILED
            }
        }
    }

    /// Drive a tone into a wired pad's coils (and with `--speaker`, its speaker), so
    /// "the plane never arrived" separates from "it arrived and the graph folded the
    /// coil pair away". Deliberately blind to the settings, which is why it says up
    /// front when a real session would render nothing.
    #[cfg(target_os = "linux")]
    fn pad_audio_devtest() -> u8 {
        let seconds = arg_value("--seconds")
            .and_then(|v| v.parse().ok())
            .unwrap_or(3);
        // Coils by default: they are the half that silently disappears, so they are the
        // half worth testing. `--speaker` adds (or, with nothing else, selects) the
        // speaker pair.
        let speaker = arg_flag("--speaker");
        let coils = arg_flag("--coils") || !speaker;
        // This drives the pad DIRECTLY and ignores the settings, so say up front when a
        // real session would render nothing: "the tone plays but the game is silent" sends
        // you measuring the host, and the toggle is on the client. Nothing logs it later —
        // the capability is never advertised when the toggle is off.
        {
            let s = trust::Settings::load();
            if speaker && !pf_client_core::pad_audio::speaker_active(&s.pad_speaker) {
                println!(
                    "note: \"Controller speaker\" is OFF in your settings (pad_speaker = \
                     {:?}), so a streaming session will NOT render the pad's speaker even if \
                     the tone below is audible.",
                    s.pad_speaker
                );
            }
            if coils && !s.pad_haptics {
                println!(
                    "note: \"Controller haptics\" is OFF in your settings, so a streaming \
                     session will NOT render the voice coils even if the tone below is felt."
                );
            }
        }
        match pf_client_core::pad_audio::pad_audio_test(seconds, coils, speaker) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("pad-audio-test: {e:#}");
                EXIT_PRESENTER_FAILED
            }
        }
    }

    /// Per-adapter Vulkan Video decode capability, in human words — a triage tool, not a
    /// picker source. That is why it is its own flag: `--list-adapters` is parsed
    /// line-by-line by the desktop shells' GPU picker and must keep printing bare names.
    fn probe_decode_report() -> u8 {
        match pf_presenter::vk::probe_decode() {
            Ok(adapters) => {
                if adapters.is_empty() {
                    println!("no Vulkan physical devices");
                }
                for (i, a) in adapters.iter().enumerate() {
                    // The bracketed number is the PUNKTFUNK_VK_DEVICE value, and the
                    // FIRST listed entry is what
                    // a default run presents on — the decoder shares that device, so
                    // on a hybrid box this line is usually the answer.
                    let kind = if a.discrete { "discrete" } else { "integrated" };
                    // `a.index`, NOT the loop position: this list is sorted
                    // discrete-first, while PUNKTFUNK_VK_DEVICE indexes the raw
                    // enumeration, which puts the iGPU first on some hybrids. The
                    // `i == 0` marker stays the loop position — sorted-first is what
                    // `pick_device` lands on when nothing overrides it.
                    println!(
                        "[{}] {} ({kind}){}",
                        a.index,
                        a.name,
                        if i == 0 { "  <- default presenter" } else { "" }
                    );
                    println!(
                        "     vulkan video decode: {}",
                        if a.usable { "YES" } else { "no" }
                    );
                    // Name every advertised bit, including unsupported VP9, so the labels
                    // account for the complete mask.
                    const OPS: [(u32, &str); 4] = [
                        (0x1, "H.264"),
                        (0x2, "H.265"),
                        (0x4, "AV1"),
                        (0x8, "VP9 (no punktfunk rung)"),
                    ];
                    let mut codecs: Vec<String> = OPS
                        .iter()
                        .filter(|(bit, _)| a.codec_ops & bit != 0)
                        .map(|(_, n)| (*n).to_string())
                        .collect();
                    let named: u32 = OPS.iter().map(|(b, _)| b).sum();
                    let unknown = a.codec_ops & !named;
                    if unknown != 0 {
                        codecs.push(format!("unrecognised bits 0x{unknown:X}"));
                    }
                    println!(
                        "     driver decode ops:   {}",
                        if codecs.is_empty() {
                            format!("none (0x{:X})", a.codec_ops)
                        } else {
                            format!("{} (0x{:X})", codecs.join(", "), a.codec_ops)
                        }
                    );
                    if !a.usable {
                        // Say which conjunct failed. "no" with no reason is the thing
                        // this whole flag exists to stop.
                        let mut why: Vec<String> = Vec::new();
                        if !a.api_1_3 {
                            why.push("device is not Vulkan 1.3".into());
                        }
                        if !a.features_ok {
                            why.push(
                                "missing samplerYcbcrConversion / timelineSemaphore / \
                                 synchronization2"
                                    .into(),
                            );
                        }
                        if a.decode_family.is_none() {
                            why.push("no queue family advertises VIDEO_DECODE".into());
                        }
                        if !a.base_missing.is_empty() {
                            why.push(format!("missing {}", a.base_missing.join(", ")));
                        }
                        if a.codec_exts.is_empty() {
                            why.push("no VK_KHR_video_decode_{h264,h265,av1} extension".into());
                        }
                        println!("     why not:             {}", why.join("; "));
                    } else {
                        println!("     extensions:          {}", a.codec_exts.join(", "));
                    }
                    print_video_formats(a);
                }
                if adapters.len() > 1 {
                    // The single most common misreading of this output: seeing a
                    // capable GPU listed and concluding the decoder will use it.
                    // Vulkan Video decodes on the PRESENTER's device, and the decoder
                    // preference does not move the presenter.
                    println!();
                    println!(
                        "Vulkan Video decodes on the presenter's device. PUNKTFUNK_DECODER \
                         picks the rung,"
                    );
                    println!(
                        "not the GPU — move the presenter with PUNKTFUNK_VK_DEVICE=<index \
                         above> or"
                    );
                    println!(
                        "PUNKTFUNK_VK_ADAPTER=<name substring>, which is the safer knob \
                         where two"
                    );
                    println!("adapters share a name.");
                }
                0
            }
            Err(e) => {
                eprintln!("probe-decode: {e:#}");
                EXIT_PRESENTER_FAILED
            }
        }
    }

    /// Mesa gates Vulkan Video decode — the `VK_KHR_video_decode_*` extensions AND the
    /// decode-capable queue family — behind one switch per driver: RADV reads
    /// `RADV_PERFTEST=video_decode`, ANV (Intel) `ANV_DEBUG=video-decode`. Without it the
    /// presenter's device advertises no decode queue and `auto` falls to VAAPI, which
    /// chroma-fringes on VanGogh and is unverified on Intel. Every other driver ignores
    /// both, and a driver that already decodes by default no-ops. Appended, never
    /// clobbered, so a user's own flags survive; `PUNKTFUNK_DECODER=native-vaapi` still
    /// pins VAAPI.
    ///
    /// ⚠⚠ Called from the TOP of [`run`], ahead of the `--list-adapters` / `--probe-decode`
    /// early exits. Those create Vulkan instances of their own and Mesa latches these
    /// variables when its ICD initialises, so a later call leaves the triage tools
    /// describing a device that cannot decode while the streaming path decodes on it.
    #[cfg(target_os = "linux")]
    #[allow(unsafe_code)] // the SAFETY-commented single-threaded-startup env write below
    fn enable_mesa_video_decode() {
        for (var, token) in [
            ("RADV_PERFTEST", "video_decode"),
            ("ANV_DEBUG", "video-decode"),
        ] {
            let Some(value) = with_token(std::env::var(var).ok().as_deref(), token) else {
                continue;
            };
            // SAFETY: called at the very top of `run()`, before this process creates any
            // thread — the Vulkan loader, SDL, and the session runtime all start later.
            unsafe { std::env::set_var(var, &value) };
            tracing::info!(var, value = %value, "opted into Mesa Vulkan Video decode");
        }
    }

    /// `current` with `token` appended to its comma list; `None` when it is already there.
    #[cfg(target_os = "linux")]
    fn with_token(current: Option<&str>, token: &str) -> Option<String> {
        match current {
            Some(v) if v.split(',').any(|t| t == token) => None,
            Some(v) if !v.is_empty() => Some(format!("{v},{token}")),
            _ => Some(token.to_owned()),
        }
    }

    /// The driver's own answers about video images, printed with nothing in front of
    /// them (`--probe-decode`).
    ///
    /// Passing the five conjuncts above only says Vulkan Video EXISTS on a device; this
    /// says whether the zero-copy pipeline can be BUILT on it — a different question
    /// with, on at least one shipping driver, a different answer. Verbatim on purpose:
    /// the Intel Arc refusal was twice diagnosed from punktfunk's own error text and
    /// twice the diagnosis was wrong, and what broke it open both times was reading what
    /// the driver actually said.
    fn print_video_formats(a: &pf_presenter::vk::AdapterDecode) {
        use pf_presenter::vk::probe::{describe_create_flags, describe_usage};
        for p in &a.formats {
            println!("     {} (wants {:?}):", p.profile, p.wanted);
            for u in &p.usages {
                let answer = match &u.formats {
                    Err(e) => format!("query failed: {e:?}"),
                    Ok(entries) if entries.is_empty() => "no formats offered".to_string(),
                    Ok(entries) => entries
                        .iter()
                        .map(|f| {
                            format!(
                                "{:?} usage={} create={} {:?} {:?}",
                                f.format,
                                describe_usage(f.image_usage),
                                describe_create_flags(f.image_create_flags),
                                f.image_type,
                                f.image_tiling,
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("; "),
                };
                println!("       {:<24} {answer}", u.label);
                // The second opinion, printed only where it differs from the video format
                // query. This call does not honour the profile list, so it answers
                // "creatable" for combinations the video query rejects; a difference is
                // not the driver contradicting itself.
                let listed = u
                    .wanted_entry(p.wanted)
                    .is_some_and(|f| f.image_usage.contains(u.usage));
                if listed != u.image_format_support.is_ok() {
                    let second = match &u.image_format_support {
                        Ok(()) => "creatable".to_string(),
                        Err(e) => format!("{e:?}"),
                    };
                    println!(
                        "       {:<24} (also asked: \
                         vkGetPhysicalDeviceImageFormatProperties2 says {second} — that \
                         call does not honour the profile list; not authority)",
                        ""
                    );
                }
            }
        }
    }

    pub fn run() -> u8 {
        // Logs to STDERR — stdout is the machine interface (ready/stats/error lines) — plus
        // the in-process ring (`pf_client_core::logring`, DEBUG+ regardless of RUST_LOG) that
        // "Send logs to host" uploads. The env filter scopes the STDERR layer only: the ring
        // exists precisely for the diagnostics nobody enabled before the bug happened.
        {
            use tracing_subscriber::layer::SubscriberExt;
            use tracing_subscriber::util::SubscriberInitExt;
            use tracing_subscriber::Layer;
            tracing_subscriber::registry()
                .with(
                    tracing_subscriber::fmt::layer()
                        .with_writer(std::io::stderr)
                        .with_filter(
                            tracing_subscriber::EnvFilter::try_from_default_env()
                                .unwrap_or_else(|_| "info".into()),
                        ),
                )
                .with(
                    pf_client_core::logring::RingLayer
                        .with_filter(tracing_subscriber::filter::LevelFilter::DEBUG),
                )
                .init();
        }
        // SEH last-resort: a driver AV otherwise leaves only an exit code in the shell's log.
        #[cfg(windows)]
        punktfunk_core::crash::install();

        // Runs before ANY Vulkan call, including the probe flags below — hence the top of
        // `run`, ahead of the early exits, so triage answers the same question the streaming
        // path asks. Makes RADV and ANV expose their video-decode queue and extensions so the
        // decoder's `auto` path can take Vulkan Video. Windows drivers expose theirs already.
        #[cfg(target_os = "linux")]
        enable_mesa_video_decode();

        // `--list-adapters`: print the Vulkan physical devices' marketing names (one per
        // line, discrete first) for the desktop shells' GPU picker, then exit.
        if arg_flag("--list-adapters") {
            return match pf_presenter::vk::list_adapters() {
                Ok(names) => {
                    for n in names {
                        println!("{n}");
                    }
                    0
                }
                Err(e) => {
                    eprintln!("list-adapters: {e:#}");
                    EXIT_PRESENTER_FAILED
                }
            };
        }

        if arg_flag("--probe-decode") {
            return probe_decode_report();
        }

        // `--list-audio`: the PipeWire endpoints the settings pickers offer, as
        // `sink|source<TAB>node.name<TAB>description` lines — a debug window into the
        // same enumeration the GTK shell probes.
        #[cfg(target_os = "linux")]
        if arg_flag("--list-audio") {
            return list_audio();
        }

        // `--pad-audio-test [--seconds N] [--speaker] [--coils]`: the controller-audio
        // correlation, printed, then a tone driven into the pad. The one tool that separates
        // "the plane never arrived" from "it arrived and the graph folded the coil pair away"
        // — no host, no game, no pairing needed, just a wired DualSense.
        #[cfg(target_os = "linux")]
        if arg_flag("--pad-audio-test") {
            return pad_audio_devtest();
        }

        // Deprecated compatibility path: stdin only, so the PIN never enters process metadata.
        if let Some(pin_arg) = arg_value("--pair") {
            if pin_arg != "-" {
                eprintln!(
                    "punktfunk-session pairing accepts only `--pair -`; prefer `punktfunk pair`"
                );
                return EXIT_CONNECT_FAILED;
            }
            let mut pin = String::new();
            if std::io::stdin().read_line(&mut pin).is_err() || pin.trim().is_empty() {
                eprintln!("no pairing PIN on stdin");
                return EXIT_CONNECT_FAILED;
            }
            return headless_pair(pin.trim());
        }

        // (The RADV video-decode opt-in that used to live here now runs at the very top of
        // `run` — it has to precede the probe flags too, not just the session.)

        // Settings device picks (adapter marketing name, PipeWire node names / WASAPI
        // endpoint ids) → env unless the user already set the var, before any Vulkan
        // call — covers `--connect` and `--browse`. With a spec the values come from the
        // spec's settings, never the store (§5 zero-store-reads); lenient parsing is safe
        // because `--connect` re-reads the spec authoritatively and errors there.
        {
            let s = arg_value("--resolved-spec")
                .and_then(|p| {
                    pf_client_core::orchestrate::ResolvedSpec::read(std::path::Path::new(&p)).ok()
                })
                .map_or_else(trust::Settings::load, |spec| spec.settings);
            for (var, value) in [
                ("PUNKTFUNK_VK_ADAPTER", &s.adapter),
                ("PUNKTFUNK_AUDIO_SINK", &s.speaker_device),
                ("PUNKTFUNK_AUDIO_SOURCE", &s.mic_device),
            ] {
                if std::env::var_os(var).is_none() && !value.is_empty() {
                    // SAFETY: still the single-threaded startup stretch of `run()` — the
                    // early-exit probes above return out of the process, and everything that
                    // spawns threads (the session, the console, SDL) only starts below.
                    #[allow(unsafe_code)]
                    unsafe {
                        std::env::set_var(var, value)
                    };
                }
            }
        }

        // Steam launches its shortcuts with SDL_GAMECONTROLLER_IGNORE_DEVICES naming
        // every pad Steam Input has virtualized; capturing the Deck's real built-in
        // controller needs it cleared (same rationale as the GTK client's `app::run`).
        for var in [
            "SDL_GAMECONTROLLER_IGNORE_DEVICES",
            "SDL_GAMECONTROLLER_IGNORE_DEVICES_EXCEPT",
        ] {
            if let Ok(v) = std::env::var(var) {
                tracing::info!(var, value = %v, "clearing Steam's SDL device filter");
                // SAFETY: as the settings block above — single-threaded startup, before SDL
                // (the reader of these variables) or any other thread exists.
                #[allow(unsafe_code)]
                unsafe {
                    std::env::remove_var(var)
                };
            }
        }

        if arg_flag("--browse") {
            // Bare `--browse` opens the console home (hosts, pairing, settings);
            // `--browse host[:port]` opens straight into that host's library.
            let target = arg_value("--browse");
            #[cfg(feature = "ui")]
            return crate::console::run(target.as_deref());
            #[cfg(not(feature = "ui"))]
            {
                let _ = target;
                eprintln!(
                    "--browse needs the console UI — this is the minimal build \
                     (rebuild without --no-default-features)"
                );
                return EXIT_PRESENTER_FAILED;
            }
        }
        let Some(target) = arg_value("--connect") else {
            eprintln!(
                "usage: punktfunk-session --connect host[:port] [--fp HEX] [--launch id] [--preset REF] [--fullscreen]\n\
                 \x20      punktfunk-session --browse [host[:port]] [--mgmt PORT] [--fullscreen] [--json-status]\n\
                 \x20      punktfunk-session --pair - --connect host[:port] [--name LABEL]\n\
                 \n\
                 Streams from a paired punktfunk host in a Vulkan window. --browse opens the\n\
                 gamepad console instead: bare --browse is the host list (discovery, PIN\n\
                 pairing, settings, wake-on-LAN); with a target it opens that host's game\n\
                 library. --preset picks a preset by id or name for this session\n\
                 only (\"\" = the global defaults); without it the host's own preset applies.\n\
                 --connect never dials a host it has no pinned fingerprint for —\n\
                 enrol with --pair (no display needed), in the console, or from the desktop\n\
                 client."
            );
            return EXIT_CONNECT_FAILED;
        };
        let (addr, port) = parse_host_port(&target);

        let identity = match trust::load_or_create_identity() {
            Ok(i) => i,
            Err(e) => {
                tracing::error!(error = %format!("{e:#}"), "loading the client identity");
                json_line("error", "this device's client key didn't load", None);
                return EXIT_CONNECT_FAILED;
            }
        };
        // `--resolved-spec <path>`: the spawner already did the resolving, so this process
        // performs ZERO store reads (design/client-architecture-split.md §5) — no Settings
        // load, no known-hosts lookup, no preset resolution. Without it (a hand-run
        // `--connect`, an old Decky script) the session resolves for itself through the SAME
        // helper, so the two modes cannot drift.
        let spec = arg_value("--resolved-spec").map(std::path::PathBuf::from);
        // `--fp` names its own record; only a bare address falls back to what it answers with.
        let fp_arg = arg_value("--fp").map(|f| f.to_ascii_lowercase());
        let resolved = match &spec {
            Some(path) => match ResolvedSpec::read(path) {
                Ok(s) => {
                    tracing::info!(path = %path.display(), "running from a resolved spec");
                    Some(s)
                }
                Err(e) => {
                    tracing::error!(error = %e, path = %path.display(), "reading the resolved spec");
                    json_line("error", "this stream's settings didn't load", None);
                    return EXIT_CONNECT_FAILED;
                }
            },
            None => None,
        };
        let (settings, preset_name, preset_id) = match &resolved {
            Some(s) => (s.settings.clone(), s.preset.clone(), s.preset_id.clone()),
            None => {
                let (settings, preset) = trust::effective_settings(
                    fp_arg.as_deref(),
                    &addr,
                    port,
                    preset_arg().as_deref(),
                    arg_value("--launch").as_deref(),
                );
                let id = preset.as_ref().map(|p| p.id.clone());
                (settings, preset.map(|p| p.name), id)
            }
        };
        if let Some(name) = &preset_name {
            tracing::info!(preset = %name, "streaming with a settings preset");
        }

        // Trust follows the GTK client's `--connect` rules: a stored (or `--fp`) pin
        // connects silently; an unknown host is REFUSED — there is no dialog here, and a
        // silent TOFU would defeat the pinning model. Pair via the desktop client.
        let known = trust::KnownHosts::load();
        let known_host = known.resolve(fp_arg.as_deref(), &addr, port);
        let pin = fp_arg
            .as_deref()
            .and_then(trust::parse_hex32)
            .or_else(|| known_host.and_then(|h| trust::parse_hex32(&h.fp_hex)));
        let Some(pin) = pin else {
            json_line(
                "error",
                &format!("{addr}:{port} isn't paired with this device yet. Pair it to continue."),
                Some(true),
            );
            return EXIT_TRUST_REJECTED;
        };

        let host_label = known_host.map_or_else(|| addr.clone(), |h| h.name.clone());
        let launch = arg_value("--launch");
        let title = launch
            .clone()
            .map_or_else(|| host_label.clone(), |id| format!("{host_label} · {id}"));

        // `--fullscreen` carries the client's own `fullscreen_on_stream`, so the env may only
        // ADD to it under a real gamescope — reading it loosely made every flatpak stream
        // fullscreen no matter what the setting said.
        let fullscreen = arg_flag("--fullscreen") || gaming_mode();

        let opts = pf_presenter::SessionOpts {
            window_title: format!("Punktfunk · {title}"),
            fullscreen,
            window_pos: window_pos(),
            stats_verbosity: stats_tier(&settings),
            touch_mode: settings.touch_mode(),
            mouse_mode: settings.mouse_mode(),
            invert_scroll: settings.invert_scroll,
            inhibit_shortcuts: settings.inhibit_shortcuts,
            overlay_actions: settings.overlay_actions.clone(),
            present_priority: settings.present_priority(),
            vsync: settings.vsync,
            allow_vrr: settings.allow_vrr,
            json_status: true,
            on_connected: Some(Box::new(|fingerprint: [u8; 32], mgmt_port: u16| {
                let fp = trust::hex(&fingerprint);
                // This host's card carries the accent bar in the desktop client now.
                trust::touch_last_used(&fp);
                // Save where this host serves its library, learned from the session's own
                // Welcome rather than an mDNS advert — so it keeps working on a network where
                // discovery never does. `0` = the host advertised none; leave what we have.
                if mgmt_port != 0 {
                    trust::learn_mgmt_port_by_fp(&fp, mgmt_port);
                }
            })),
            // The Skia console UI (stats OSD, capture HUD) — compiled out of the
            // power-user build (`--no-default-features` drops the `ui` feature).
            #[cfg(feature = "ui")]
            overlay: Some(Box::new(pf_console_ui::SkiaOverlay::new())),
            #[cfg(not(feature = "ui"))]
            overlay: None,
            window_size: window_size(&settings),
            // A spawned session (spec mode) reports its window; a hand-run one persists it.
            match_window: match_window(&settings, spec.is_none()),
            render_scale: settings.render_scale,
            render_scale_max_dim: punktfunk_core::render_scale::max_dimension(&settings.codec),
            video_fit: punktfunk_core::video_fit::VideoFit::from_name(&settings.video_fit),
        };

        let outcome =
            pf_presenter::run_session(opts, move |gamepad, native, hdr, force_software, vulkan| {
                match resolved {
                    Some(spec) => params_from_spec(
                        spec,
                        addr,
                        port,
                        pin,
                        identity,
                        launch,
                        gamepad,
                        native,
                        hdr,
                        force_software,
                        vulkan,
                    ),
                    None => session_params(
                        &settings,
                        preset_name,
                        preset_id,
                        None,
                        addr,
                        port,
                        pin,
                        identity,
                        launch,
                        gamepad,
                        native,
                        hdr,
                        force_software,
                        vulkan,
                    ),
                }
            });

        match outcome {
            Ok(pf_presenter::Outcome::Ended(None)) => 0,
            Ok(pf_presenter::Outcome::Ended(Some(reason))) => {
                // The host ending the session (game quit, host shutdown) is a normal end
                // for a one-shot stream binary — report the reason, exit clean.
                json_line("ended", &reason, None);
                0
            }
            Ok(pf_presenter::Outcome::ConnectFailed {
                msg,
                trust_rejected,
            }) => {
                json_line("error", &msg, Some(trust_rejected));
                if trust_rejected {
                    EXIT_TRUST_REJECTED
                } else {
                    EXIT_CONNECT_FAILED
                }
            }
            Err(e) => {
                tracing::error!(error = %format!("{e:#}"), "running the presenter");
                json_line("error", "the stream window didn't start", None);
                EXIT_PRESENTER_FAILED
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use trust::StatsVerbosity as V;

        /// `--stats` is a floor, never a ceiling: it lifts Off to Normal and leaves every
        /// richer chosen tier alone. Both run modes' presenter options AND the per-launch
        /// params read this one rule, which is the point of having it.
        #[test]
        fn the_stats_flag_lifts_off_and_demotes_nothing() {
            assert_eq!(stats_tier_with(V::Off, true), V::Normal);
            assert_eq!(stats_tier_with(V::Off, false), V::Off);
            for chosen in [V::Compact, V::Normal, V::Detailed] {
                assert_eq!(stats_tier_with(chosen, true), chosen);
                assert_eq!(stats_tier_with(chosen, false), chosen);
            }
        }

        #[cfg(target_os = "linux")]
        #[test]
        fn a_mesa_switch_is_appended_once_and_keeps_the_users_own() {
            assert_eq!(
                with_token(None, "video-decode").as_deref(),
                Some("video-decode")
            );
            assert_eq!(
                with_token(Some(""), "video-decode").as_deref(),
                Some("video-decode")
            );
            assert_eq!(
                with_token(Some("sync"), "video-decode").as_deref(),
                Some("sync,video-decode")
            );
            assert_eq!(with_token(Some("sync,video-decode"), "video-decode"), None);
        }

        /// The console reads the file ONCE for its window, so a tier changed between streams
        /// can only reach the overlay by riding the launch. Guards the wiring the field exists
        /// for: whatever settings a launch resolved is what the params carry.
        #[test]
        fn a_launch_carries_the_tier_its_settings_resolved() {
            let mut s = trust::Settings::default();
            for chosen in [V::Off, V::Compact, V::Normal, V::Detailed] {
                s.set_stats_verbosity(chosen);
                assert_eq!(stats_tier_with(s.stats_verbosity(), false), chosen);
            }
        }
    }
}

#[cfg(any(target_os = "linux", windows))]
fn main() -> std::process::ExitCode {
    std::process::ExitCode::from(session_main::run())
}

/// This stub keeps `cargo build --workspace` green elsewhere (the Mac client lives in
/// clients/apple).
#[cfg(not(any(target_os = "linux", windows)))]
fn main() {
    eprintln!(
        "punktfunk-session runs on Linux and Windows — the macOS client lives in clients/apple"
    );
    std::process::exit(2);
}

#[cfg(test)]
mod tests {
    use super::resolve_hdr_enabled;

    // The stored preference cannot advertise HDR through an SDR Windows output.
    // Presentation support remains a separate required device fact.
    #[test]
    fn hdr_requires_the_setting_output_and_presenter() {
        assert!(resolve_hdr_enabled(true, true, || true));
        assert!(!resolve_hdr_enabled(false, true, || panic!()));
        assert!(!resolve_hdr_enabled(true, false, || panic!()));
        assert!(!resolve_hdr_enabled(true, true, || false));
    }
}
