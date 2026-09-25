//! The shell↔session handoff: every stream runs in the spawned `punktfunk-session`
//! Vulkan binary (the legacy in-process presenter is gone — phase 5 of
//! punktfunk-planning `linux-client-rearchitecture.md`). What is left here is the
//! TRANSLATION: a [`ConnectRequest`] becomes a `ConnectPlan`, and the session's typed
//! lifecycle events become the [`AppMsg`]s the relm4 app consumes — spinner until
//! `{"ready":true}`, banner from the `{"error"|"ended": …}` line, exit code 3 +
//! `trust_rejected` routed to the re-pair PIN ceremony.
//!
//! Spawning, the argv, the stdout contract and the child handle live in
//! `pf_client_core::orchestrate` (design/client-architecture-split.md §3) — the WinUI shell
//! and the coming CLI spawn through the same code, so "what flags does a stream get" and
//! "what does ready mean" have exactly one answer.

use crate::app::AppMsg;
use crate::ui_hosts::ConnectRequest;
use pf_client_core::orchestrate::{self, ConnectPlan, HostTarget, SessionEvent};

/// Spawn tunables beyond a plain connect.
#[derive(Debug, Default)]
pub struct SpawnOpts {
    /// Handshake budget override (`--connect-timeout`) — the request-access flow passes
    /// ~185 s because the host PARKS the connection until the operator approves.
    pub connect_timeout_secs: Option<u64>,
    /// Persist the host as *paired* once the child reports ready (request-access: the
    /// operator's approval IS the pairing). Plain TOFU persists unpaired.
    pub persist_paired: bool,
    /// A cancel handle to arm (request-access's waiting dialog): killing the child is
    /// the only abort a parked connect has.
    pub cancel: Option<CancelHandle>,
}

pub use orchestrate::{session_binary, CancelHandle};

/// The plan one card click resolves to. `for_target` does ALL of the resolving — this
/// device's settings with the host's bound preset (or the "Connect with ▸" one-off) overlaid,
/// and the per-host clipboard decision — through the same helpers the session's own compat path
/// uses, so the two cannot disagree.
///
/// Resolving is not optional: `orchestrate::spawn_session` writes `plan.settings` into the
/// child's `--resolved-spec`, and a session running from a spec reads NO stores. This used to
/// build the plan with `..Settings::default()`, on the theory that only the argv read it —
/// which meant every stream since the arch-split ran at the DEFAULTS: the host's fallback
/// bitrate (20 Mbps), the native resolution, `auto` codec, stereo, and no preset.
fn plan_for(req: &ConnectRequest, fp_hex: &str, tofu: bool, opts: &SpawnOpts) -> ConnectPlan {
    let mut plan = ConnectPlan::for_target(
        HostTarget {
            name: req.name.clone(),
            addr: req.addr.clone(),
            port: req.port,
            fp_hex: Some(fp_hex.to_string()),
            mac: req.mac.clone(),
            id: None,
            mgmt_port: None, // this shell resolves the library port itself (`mgmt_port_for`)
        },
        req.launch.as_ref().map(|(id, _)| id.clone()),
        // A plain card click carries no one-off: the resolver honors the host's own binding
        // (design/client-settings-profiles.md §4.6). Only a "Connect with ▸" pick (or a URL's
        // `preset=`) sets one, and it applies to this session alone.
        req.preset.clone(),
    );
    // This shell still runs its own dial-first wake fallback in `app.rs`, so the plan's own
    // wake stays off (it becomes real when the GTK connect path moves onto
    // `ConnectOrchestrator`, arch-split C0).
    plan.wake = false;
    plan.connect_timeout_secs = opts.connect_timeout_secs;
    plan.tofu = tofu;
    plan
}

/// Spawns the session with `fp_hex` pinned and translates its lifecycle into
/// [`AppMsg`]s. A ready event carries the request's cancel handle so the main loop
/// rejects approval that was queued while Cancel ran.
///
/// `tofu` persists an advertised fingerprint only after ready proves the host owns it.
/// The plan supplies all effective settings, including preset fullscreen policy.
///
/// The caller takes `busy`; [`AppMsg::SessionExited`] releases it. `Err` reports a
/// spawn failure before supervision starts.
pub fn spawn_session(
    sender: relm4::Sender<AppMsg>,
    req: ConnectRequest,
    fp_hex: String,
    tofu: bool,
    opts: SpawnOpts,
) -> Result<(), String> {
    let plan = plan_for(&req, &fp_hex, tofu, &opts);
    let persist_paired = opts.persist_paired;
    let cancel = opts.cancel.clone();
    let (mut error, mut ended) = (None::<(String, bool)>, None::<String>);
    orchestrate::spawn_session(&plan, opts.cancel, move |ev| match ev {
        SessionEvent::Ready => {
            let _ = sender.send(AppMsg::SessionReady {
                req: req.clone(),
                fp_hex: fp_hex.clone(),
                tofu,
                persist_paired,
                cancel: cancel.clone(),
            });
        }
        SessionEvent::Error {
            msg,
            trust_rejected,
        } => error = Some((msg, trust_rejected)),
        SessionEvent::Ended(msg) => ended = Some(msg),
        // The brain persists the window size; the shell has nothing to do with it.
        SessionEvent::Window { .. } => {}
        SessionEvent::Exited(code) => {
            let _ = sender.send(AppMsg::SessionExited {
                req: req.clone(),
                code,
                error: error.take(),
                ended: ended.take(),
                tofu,
            });
        }
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The plan a card click builds carries THIS DEVICE'S settings, with the host's bound
    /// preset overlaid — not `Settings::default()`.
    ///
    /// This is the 0.22.x regression, and it was invisible because the defaults are all
    /// plausible: the spec's `bitrate_kbps: 0` means "host default", which the host reads as
    /// its 20 Mbps fallback. A stream that ignores every setting looks exactly like a stream
    /// that is merely capped. One test, one directory — the stores are read from it, so this
    /// deliberately does not split into several that would race over the same variable.
    #[test]
    // The crate's one test env mutation (the config-dir write below) — see main.rs's deny note.
    #[allow(unsafe_code)]
    fn the_plan_carries_resolved_settings_not_defaults() {
        use pf_client_core::presets::{PresetsFile, SettingsOverlay, StreamPreset};
        use pf_client_core::trust::{KnownHost, KnownHosts, Settings};

        let home = std::env::temp_dir().join(format!("pf-spawn-test-{}", std::process::id()));
        let cfg = home.join(".config/punktfunk");
        std::fs::create_dir_all(&cfg).unwrap();
        // SAFETY: the only env-mutating test in this binary (see the doc above — one test, one
        // directory, deliberately not split). Parallel tests may `getenv` concurrently; glibc
        // keeps replaced environ storage alive, and every reader tolerates either value.
        // Point the store at `cfg` directly. A `HOME` redirect loses to a developer override.
        unsafe { std::env::set_var("PUNKTFUNK_CONFIG_DIR", &cfg) };

        // A device whose owner has set a bitrate, and a host bound to a preset that raises it
        // further — the two layers the spec has to carry.
        let globals = Settings {
            bitrate_kbps: 200_000,
            width: 2560,
            height: 1440,
            codec: "av1".into(),
            fullscreen_on_stream: false,
            ..Default::default()
        };
        globals.save();
        let mut catalog = PresetsFile {
            version: 1,
            presets: vec![StreamPreset {
                id: "aaaaaaaaaaaa".into(),
                name: "Game".into(),
                overrides: SettingsOverlay {
                    bitrate_kbps: Some(750_000),
                    ..Default::default()
                },
                ..StreamPreset::new("")
            }],
        };
        catalog.save().unwrap();
        let known = KnownHosts {
            hosts: vec![KnownHost {
                name: "Desk".into(),
                addr: "192.168.1.50".into(),
                port: 9777,
                fp_hex: "a".repeat(64),
                paired: true,
                preset_id: Some("aaaaaaaaaaaa".into()),
                clipboard_sync: true,
                ..Default::default()
            }],
        };
        known.save().unwrap();

        let req = ConnectRequest {
            name: "Desk".into(),
            addr: "192.168.1.50".into(),
            port: 9777,
            fp_hex: Some("a".repeat(64)),
            pair_optional: false,
            launch: None,
            mac: vec![],
            preset: None,
        };
        let opts = SpawnOpts::default();

        // A card click: globals, with the BOUND preset's overrides on top.
        let plan = plan_for(&req, &"a".repeat(64), false, &opts);
        assert_eq!(plan.settings.bitrate_kbps, 750_000, "preset override");
        assert_eq!((plan.settings.width, plan.settings.height), (2560, 1440));
        assert_eq!(plan.settings.codec, "av1");
        assert_eq!(plan.preset.as_ref().map(|p| p.name.as_str()), Some("Game"));
        assert!(plan.clipboard, "the host's own opt-in");
        // …and the spec the child actually runs from is those same settings.
        assert_eq!(plan.spec(plan.clipboard).settings, plan.settings);
        // The fullscreen policy rides the argv from the resolved settings, not a shell global.
        assert!(!plan.session_args().contains(&"--fullscreen".to_string()));

        // "Connect with ▸ Default settings" (`Some("")`) drops back to the globals, and the
        // flag still rides so the session agrees about which layer it is on.
        let defaults_req = ConnectRequest {
            preset: Some(String::new()),
            ..req.clone()
        };
        let plan = plan_for(&defaults_req, &"a".repeat(64), false, &opts);
        assert_eq!(plan.settings.bitrate_kbps, 200_000, "back to the globals");
        assert_eq!(plan.preset, None);
        let args = plan.session_args();
        let i = args.iter().position(|a| a == "--preset").unwrap();
        assert_eq!(args[i + 1], "");

        let _ = std::fs::remove_dir_all(&home);
    }
}
