//! `serve`'s command line: [`parse_serve_args`] reads the flags and their env fallbacks and
//! writes nothing; [`prepare_serve`] then mints the credentials, converges the plugin grants and
//! publishes the endpoint a serving host needs.

use crate::{discovery, mgmt, native, webtransport};
use anyhow::{bail, Result};
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

/// Native plane + management API always run. `--gamestream` is trusted-LAN only.
/// Pairing is required unless `--open`. A bad flag or env value is an error.
pub(crate) fn parse_serve_args(args: &[String]) -> Result<ServeArgs> {
    let mut mgmt_bind = None;
    let mut native_port: u16 = 9777;

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
    // Explicit `--native-port` outranks `PUNKTFUNK_NATIVE_PORT` after the loop.
    let mut native_port_explicit = false;
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
                native_port = next()?
                    .parse()
                    .map_err(|_| anyhow::anyhow!("bad --native-port (want a port number)"))?;
                native_port_explicit = true;
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
    // Default all-interfaces so paired clients browse over mTLS. Admin stays loopback in
    // `require_auth`. Packaged units ship a fixed ExecStart — `host.env` is the upgrade-safe pin;
    // CLI wins as the more explicit of the two.
    let mgmt_bind = match mgmt_bind {
        Some(bind) => bind,
        None => match pf_host_config::config().mgmt_bind.as_deref() {
            Some(s) => s
                .parse()
                .map_err(|_| anyhow::anyhow!("bad PUNKTFUNK_MGMT_BIND '{s}' (want IP:PORT)"))?,
            None => SocketAddr::from(([0, 0, 0, 0], mgmt::DEFAULT_PORT)),
        },
    };
    // A bad value is fatal — serving 9777 while host.env says otherwise reads as
    // "I moved the port and the client still cannot reach me".
    if !native_port_explicit {
        if let Some(s) = pf_host_config::config().native_port.as_deref() {
            native_port = s
                .parse()
                .map_err(|_| anyhow::anyhow!("bad PUNKTFUNK_NATIVE_PORT '{s}' (want a port)"))?;
        }
    }
    let native = native::NativeServe {
        port: native_port,
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
/// any runner grant.
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
    // The tray's bearer. A seat host has no tray and serves no summary. Not fatal: the tray
    // then shows the host as running without detail.
    #[cfg(target_os = "windows")]
    let seat_host = pf_paths::seat::is_seat_host();
    #[cfg(not(target_os = "windows"))]
    let seat_host = false;
    if !seat_host {
        match crate::mgmt_token::mint_tray_token() {
            Ok(t) => opts.tray_token = Some(t),
            Err(e) => tracing::warn!(error = %format!("{e:#}"), "tray token not written"),
        }
        crate::tray_autostart::converge();
    }
    // Mint only if the runner is installed — otherwise a second admin-adjacent credential sits
    // on disk for a subsystem that is not running. Scope: `plugin_may_access`, not pairing/hooks.
    let runner = crate::plugins::runtime_status();
    // The plugin runner is the owner's: the door has none to hand a token to.
    if runner.installed && !pf_paths::seat::is_door() {
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
        // A launcher installed after the host started brings a declared folder with no ACE; the
        // converge places it within a minute, while the plugin keeps running.
        #[cfg(windows)]
        let _ = std::thread::Builder::new()
            .name("plugin-roots".into())
            .spawn(|| loop {
                std::thread::sleep(std::time::Duration::from_secs(60));
                crate::plugins::converge_runner_roots();
            });
        crate::plugins::converge_runner_acls(&runner);
        crate::plugins::converge_seat_denies();
    }
    // Same function as the token persist so the console unit sees both. A race falls back to
    // 47990; `Restart=always` retries.
    mgmt::publish_endpoint(opts.bind);
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
    let gamestream = pf_host_config::config().gamestream && !pf_paths::seat::is_door();
    let native = native::NativeServe {
        webtransport_bind: pf_host_config::config()
            .webtransport
            .then_some(args.webtransport_bind),
        ..args.native
    };
    // A launcher rewrite can drop a granted folder's ACE; re-apply them each boot.
    crate::plugins::converge_grants();
    Ok((opts, native, gamestream))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::args;

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
