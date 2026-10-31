//! `serve`'s command line: [`parse_serve_args`] reads the flags and their env fallbacks and
//! writes nothing; [`prepare_serve`] then mints the credentials, converges the plugin grants and
//! publishes the endpoint a serving host needs.

use crate::{discovery, mgmt, native, webtransport};
use anyhow::{bail, Result};
use pf_paths::seat::{HostRole, SeatRole};
use std::net::SocketAddr;

/// `serve`'s flags with the env fallbacks applied.
pub(crate) struct ServeArgs {
    mgmt_bind: SocketAddr,
    /// The native plane; its browser-plane bind is set once the flags are pinned.
    native: native::NativeServe,
    /// Where the browser plane listens when it is on.
    webtransport_bind: SocketAddr,
    /// `--gamestream` and `--webtransport` as given; [`prepare_serve`] pins them.
    gamestream: bool,
    webtransport: bool,
}

/// The port flags `role` binds by. A seat host's are its row's, which the supervisor names in
/// the environment: a flag on the unit's command line, as a Steam Deck install writes, would keep
/// the box's ports from the door.
fn contract_ports(
    role: HostRole,
    mgmt: Option<SocketAddr>,
    native: Option<u16>,
) -> (Option<SocketAddr>, Option<u16>) {
    if !role.is_seat_host() {
        return (mgmt, native);
    }
    for (setting, set) in [
        ("--mgmt-bind", mgmt.is_some()),
        ("--native-port", native.is_some()),
    ] {
        if set {
            tracing::warn!(
                setting,
                reason = "a seat host listens on its row's ports",
                "seat-contract setting ignored"
            );
        }
    }
    (None, None)
}

/// Native plane + management API always run. `--gamestream` is trusted-LAN only.
/// Pairing is required unless `--open`. A bad flag or env value is an error.
pub(crate) fn parse_serve_args(args: &[String]) -> Result<ServeArgs> {
    let mut mgmt_bind = None;
    let mut native_port = None;

    let mut open = false;
    let mut gamestream = false;
    // The browser plane, off unless asked for — same stance as GameStream above.
    let mut webtransport = false;
    let mut webtransport_port: u16 = webtransport::DEFAULT_PORT;
    let mut webtransport_port_explicit = false;
    // Interface only; the port is its own flag so `--webtransport-bind` reads like an address.
    let mut webtransport_host = "::".to_string();
    let mut webtransport_bind_explicit = false;
    let mut no_mdns = false;
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        let mut next = || {
            i += 1;
            args.get(i)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("missing value for {arg}"))
        };
        match arg {
            "--mgmt-bind" => {
                mgmt_bind = Some(
                    next()?
                        .parse()
                        .map_err(|_| anyhow::anyhow!("bad --mgmt-bind (want IP:PORT)"))?,
                );
            }
            // No-op: the native plane always runs.
            "--native" => {}
            "--native-port" => {
                native_port = Some(
                    next()?
                        .parse()
                        .map_err(|_| anyhow::anyhow!("bad --native-port (want a port number)"))?,
                );
            }
            // Video rides the native port. Accepted so an older service unit still starts.
            "--data-port" => {
                next()?;
                tracing::warn!("--data-port is ignored: video uses the native port");
            }
            "--gamestream" | "--moonlight" => gamestream = true,
            "--webtransport" => webtransport = true,
            "--webtransport-port" => {
                webtransport_port = next()?
                    .parse()
                    .map_err(|_| anyhow::anyhow!("bad --webtransport-port (want a port number)"))?;
                webtransport_port_explicit = true;
            }
            "--webtransport-bind" => {
                webtransport_host = next()?;
                webtransport_bind_explicit = true;
            }
            "--open" => open = true,
            // Read by `real_main` before startup; accepted here so it parses.
            "--door" => {}
            // Bridged Docker / CI netns: multicast never arrives.
            "--no-mdns" => no_mdns = true,
            "-h" | "--help" => {
                crate::print_usage();
                std::process::exit(0);
            }
            other => bail!("unknown argument '{other}' (try --help)"),
        }
        i += 1;
    }
    // Management defaults to every interface; admin stays loopback in `require_auth`. Packaged
    // units ship a fixed ExecStart, so `host.env` is the upgrade-safe pin, and a flag outranks
    // it. A bad env value is fatal: serving the default while host.env says otherwise reads as
    // "I moved the port and the client still can't reach me".
    let cfg = pf_host_config::config();
    let (mgmt_bind, native_port) = contract_ports(pf_paths::seat::role(), mgmt_bind, native_port);
    let listen = pf_host_config::listen::ListenPlan::resolve(
        mgmt_bind,
        native_port,
        cfg.mgmt_bind.as_deref(),
        cfg.native_port.as_deref(),
    )?;
    let mgmt_bind = listen.mgmt;
    let native = native::NativeServe {
        port: listen.native_port,
        require_pairing: !open,
        // Real bound port, not the default, so mDNS clients follow a moved mgmt port.
        mgmt_port: mgmt_bind.port(),
        mdns: !no_mdns && discovery::mdns_enabled(),
        // Set by `prepare_serve`, once the flags are pinned.
        webtransport_bind: None,
    };
    if !webtransport_port_explicit {
        if let Some(s) = pf_host_config::config().webtransport_port.as_deref() {
            webtransport_port = s.parse().map_err(|_| {
                anyhow::anyhow!("bad PUNKTFUNK_WEBTRANSPORT_PORT '{s}' (want a port)")
            })?;
        }
    }
    if !webtransport_bind_explicit {
        if let Some(s) = pf_host_config::config().webtransport_bind.as_deref() {
            webtransport_host = s.to_string();
        }
    }
    // Bracketed IPv6 or a bare IPv4, joined to the port flag. A bad address is a startup error:
    // listening on every interface when the operator asked for one is the wrong way to fail.
    let webtransport_bind: SocketAddr = format!(
        "{}:{webtransport_port}",
        if webtransport_host.contains(':') && !webtransport_host.starts_with('[') {
            format!("[{webtransport_host}]")
        } else {
            webtransport_host.clone()
        }
    )
    .parse()
    .map_err(|_| anyhow::anyhow!("bad --webtransport-bind '{webtransport_host}' (want an IP)"))?;
    // Refused here rather than at bind: the plane is spawned as a secondary tier whose errors
    // only log, and this combination must not be something an operator can miss.
    if (webtransport || pf_host_config::config().webtransport)
        && !webtransport::is_confined(
            native.require_pairing,
            &pf_host_config::config().webtransport_origins,
        )
    {
        anyhow::bail!(
            "--open leaves the browser plane unauthenticated, and with no origin list any page \
             the user visits can stream and inject input (WebTransport gets no same-origin rule). \
             Set PUNKTFUNK_WEBTRANSPORT_ORIGINS, or drop --open"
        );
    }
    Ok(ServeArgs {
        mgmt_bind,
        native,
        webtransport_bind,
        gamestream,
        webtransport,
    })
}

/// Everything `serve` writes before the planes start: the management, tray and plugin tokens,
/// the plugin ACLs and grants, the endpoint file and the flag pins. Returns
/// `(mgmt, native, gamestream)`.
///
/// The token exists before the endpoint file names it, and the config-dir ACLs converge before
/// any runner grant. A first mint of the runner's token, or a moved endpoint, marks the runner
/// for a restart.
pub(crate) fn prepare_serve(args: ServeArgs) -> Result<(mgmt::Options, native::NativeServe, bool)> {
    let mut opts = mgmt::Options {
        bind: args.mgmt_bind,
        ..Default::default()
    };
    // Env (persisted), else the `mgmt-token` file, else generate. HTTPS+token even on loopback.
    if opts.token.is_none() {
        opts.token = Some(crate::mgmt_token::load_or_generate()?);
    }
    // Installs before this build granted every local account read on the config tree.
    #[cfg(windows)]
    crate::plugins::converge_config_dir_acls();
    // The tray's bearer. Not fatal: the tray then shows the host as running without detail.
    let role = pf_paths::seat::role();
    if mints_tray_token(role) {
        match crate::mgmt_token::mint_tray_token() {
            Ok(t) => opts.tray_token = Some(t),
            Err(e) => tracing::warn!(error = %format!("{e:#}"), "tray token not written"),
        }
        crate::tray_autostart::converge();
    } else if role.is_door() {
        // Every seat account can reach the door's directory: no bearer stays there.
        let _ = std::fs::remove_file(pf_paths::config_dir().join(crate::mgmt_token::TRAY_FILE));
    }
    // Mint only if the runner is installed — otherwise a second admin-adjacent credential sits
    // on disk for a subsystem that is not running. Scope: `plugin_may_access`, not pairing/hooks.
    let runner = crate::plugins::runtime_status();
    let (mints, may_restart) = runner_token(pf_paths::seat::role());
    if runner.installed && mints {
        opts.restart_runner = may_restart
            && !pf_paths::config_dir()
                .join(crate::mgmt_token::PLUGIN_FILE)
                .exists();
        opts.plugin_token = Some(crate::mgmt_token::load_or_generate_plugin()?);
        // One token per installed plugin, so the API can tell them apart: a plugin may write its
        // own registration and its own provider, and no other's.
        opts.plugin_tokens = crate::mgmt_token::load_or_generate_per_plugin()?;
        // An upgrade or a hand-edited grants file may have changed what the runner must see.
        #[cfg(windows)]
        {
            crate::plugins::publish_sandbox_override();
            crate::plugins::recheck_runner_roots();
        }
        crate::plugins::converge_runner_roots();
        // A launcher installed after the host started brings a declared folder with no ACE, and
        // a recorded ACE can go from its folder; this puts either right within a minute.
        #[cfg(windows)]
        let _ = std::thread::Builder::new()
            .name("plugin-roots".into())
            .spawn(|| loop {
                std::thread::sleep(std::time::Duration::from_secs(60));
                crate::plugins::heal_runner_roots();
            });
        crate::plugins::converge_runner_acls(&runner);
        crate::plugins::converge_seat_denies();
    }
    // Same function as the token persist so the console unit sees both. A race falls back to
    // 47990; `Restart=always` retries. A runner up before a port move still dials the old one.
    let moved = mgmt::publish_endpoint(opts.bind);
    opts.restart_runner |= moved && runner.installed && mints && may_restart;
    // A flag outranks env and the console's value; pinning it lets the console show why.
    if args.gamestream {
        pf_host_config::pin("gamestream", "--gamestream", serde_json::Value::Bool(true));
    }
    if args.webtransport {
        pf_host_config::pin(
            "webtransport",
            "--webtransport",
            serde_json::Value::Bool(true),
        );
    }
    // The door places connects and streams nothing, and stock Moonlight has no redirect to follow.
    // A contract seat never serves it either, whatever its env says: the owner's row is the
    // box's Moonlight host, on the owner's own setting and pairings.
    let role = pf_paths::seat::role();
    let gamestream =
        pf_host_config::config().gamestream && !role.is_door() && !role.follows_contract();
    let native = native::NativeServe {
        webtransport_bind: browser_bind(
            role,
            pf_host_config::config().webtransport,
            args.webtransport_bind,
            args.native.port,
        ),
        ..args.native
    };
    Ok((opts, native, gamestream))
}

/// Where the browser plane listens. A seat host's is the browser port beside its native port, and
/// whether it runs follows the box (`webtransport::run`). Any other host's is the operator's switch.
fn browser_bind(
    role: HostRole,
    on: bool,
    bind: SocketAddr,
    native_port: u16,
) -> Option<SocketAddr> {
    if role.is_seat_host() {
        return pf_seats::model::browser_port_for(native_port)
            .map(|port| SocketAddr::new(bind.ip(), port));
    }
    on.then_some(bind)
}

/// Whether this host mints the tray's bearer. A tray runs in the desktop of the host it shows:
/// the console host's, and on Linux the owner's row, which keeps the owner's config directory.
/// The door and a contract seat have no tray, and seat accounts may not read the bearer.
fn mints_tray_token(role: HostRole) -> bool {
    match role {
        HostRole::Plain => true,
        HostRole::Seat(SeatRole::Owner | SeatRole::OwnerHeadless) => !cfg!(windows),
        HostRole::Seat(SeatRole::Contract) | HostRole::Door => false,
    }
}

/// Whether this host mints the plugin runner's token, and whether a first mint restarts the
/// runner. The runner is the owner's, so the door has none to hand a token to. A Windows seat
/// host shares the box's runner and must not bounce it.
fn runner_token(role: HostRole) -> (bool, bool) {
    match role {
        HostRole::Door => (false, false),
        HostRole::Seat(_) => (true, !cfg!(windows)),
        HostRole::Plain => (true, true),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::args;

    /// A seat's plane sits beside its native port whatever its own switch says; a seat off the
    /// pool has none, and every other host follows its switch.
    #[test]
    fn a_seat_s_browser_plane_sits_beside_its_native_port() {
        use pf_paths::seat::SeatRole;
        let bind: SocketAddr = "[::]:9778".parse().unwrap();
        let seat = HostRole::Seat(SeatRole::Contract);
        assert_eq!(
            browser_bind(seat, false, bind, 9780),
            Some("[::]:9784".parse().unwrap())
        );
        assert_eq!(browser_bind(seat, true, bind, 9800), None);
        assert_eq!(browser_bind(HostRole::Plain, true, bind, 9777), Some(bind));
        assert_eq!(browser_bind(HostRole::Door, false, bind, 9777), None);
    }

    /// The tray's bearer is the console host's and the owner's alone.
    #[test]
    fn only_a_desktop_with_a_tray_mints_its_bearer() {
        for role in HostRole::ALL {
            let owner = role.is_owner_seat() && !cfg!(windows);
            let mints = role == HostRole::Plain || owner;
            assert_eq!(mints_tray_token(role), mints, "{role:?}");
        }
    }

    #[test]
    fn the_door_mints_no_runner_token_and_a_windows_seat_restarts_no_runner() {
        for role in HostRole::ALL {
            let (mints, may_restart) = runner_token(role);
            assert_eq!(mints, !role.is_door(), "{role:?}");
            let shares_the_runner = cfg!(windows) && role.is_seat_host();
            assert!(!mints || may_restart != shares_the_runner, "{role:?}");
        }
    }

    /// Only a seat host drops its port flags: its row's environment names the ports.
    #[test]
    fn a_seat_host_listens_on_its_rows_ports_whatever_its_flags_say() {
        let mgmt: SocketAddr = "0.0.0.0:47990".parse().unwrap();
        for role in HostRole::ALL {
            let kept = contract_ports(role, Some(mgmt), Some(9777));
            let want = if role.is_seat_host() {
                (None, None)
            } else {
                (Some(mgmt), Some(9777))
            };
            assert_eq!(kept, want, "{role:?}");
        }
    }

    /// A bad flag is refused rather than replaced by the default, and so is a browser plane
    /// that `--open` would leave reachable by any page.
    #[test]
    fn serve_refuses_a_bad_value() {
        let a = parse_serve_args(&args(&[
            "--mgmt-bind",
            "127.0.0.1:47991",
            "--native-port",
            "9800",
            "--no-mdns",
        ]))
        .unwrap();
        assert_eq!(a.mgmt_bind, "127.0.0.1:47991".parse().unwrap());
        let n = &a.native;
        assert_eq!(
            (n.port, n.mgmt_port, n.require_pairing, n.mdns),
            (9800, 47991, true, false)
        );
        for bad in [
            &["--mgmt-bind", "47991"][..],
            &["--native-port", "abc"],
            &["--webtransport-port", "70000"],
            &["--webtransport-bind", "not-an-ip"],
            &["--native-port"],
            &["--frobnicate"],
            &["--webtransport", "--open"],
        ] {
            assert!(
                parse_serve_args(&args(bad)).is_err(),
                "{bad:?} was accepted"
            );
        }
    }
}
