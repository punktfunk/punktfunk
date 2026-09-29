//! Command-line entry paths: argv helpers, the headless flows (pair/wake/library), the
//! exec handoff to `punktfunk-session` for `--connect`/`--browse`.

use crate::trust::{KnownHost, KnownHosts};
use gtk::glib;
use relm4::prelude::*;

/// The value following `flag` in argv, if present (`--flag value`).
pub fn arg_value(flag: &str) -> Option<String> {
    std::env::args()
        .skip_while(|a| a != flag)
        .nth(1)
        .filter(|v| !v.starts_with("--"))
}

/// True if argv contains `flag` (a valueless switch).
pub fn arg_flag(flag: &str) -> bool {
    std::env::args().any(|a| a == flag)
}

/// A positional `punktfunk://` (or the `pf://` input alias) anywhere in argv — the deep-link
/// door (design/client-deep-links.md §4.1). It is positional, not a flag, because that is what
/// `Exec=punktfunk-client %u` hands us, what a `.desktop` shortcut embeds, and what a browser's
/// "Open Punktfunk?" prompt ends up invoking. Validation happens later, in the shared parser —
/// this only decides whether argv contains something addressed to us.
pub fn deep_link_arg() -> Option<String> {
    std::env::args()
        .skip(1)
        .find(|a| pf_client_core::deeplink::is_link_arg(a))
}

/// A bare launch under Gaming Mode opens the console, not the desktop shell. Gaming Mode
/// means gamescope, never `SteamDeck`: that variable names the machine, so it is set in
/// desktop mode too.
pub fn couch_launch() -> bool {
    cfg!(feature = "console")
        && pf_client_core::gamescope::under_gamescope()
        && deep_link_arg().is_none()
        && crate::shots::shot_scene().is_none()
}

/// Split `host[:port]`: no colon defaults the port to 9777; a colon with an unparsable
/// port yields `None` for it (callers decide whether to default or bail).
///
/// IPv6 goes through the shared parser — a plain `rsplit_once(':')` reads `::1` as host
/// `:` port `1`.
fn parse_host_port(target: &str) -> (String, Option<u16>) {
    match pf_client_core::deeplink::parse_addr_port(target) {
        Some((addr, port)) => (addr, Some(port)),
        // Keep the "colon, but no usable port" shape the callers branch on.
        None => (
            target
                .rsplit_once(':')
                .map_or(target, |(a, _)| a)
                .to_string(),
            None,
        ),
    }
}

/// `--connect` / `--browse`: streams and the console library live in the
/// `punktfunk-session` Vulkan binary — replace this process with it, forwarding the
/// relevant argv verbatim. With neither flag (a [`couch_launch`]) it opens the console home.
/// This keeps the Decky wrapper (which launches the SHELL with these flags) working unchanged
/// until it's repointed at the session binary.
pub fn exec_session() -> glib::ExitCode {
    use std::os::unix::process::CommandExt as _;
    let forward = [
        "--connect",
        "--browse",
        "--fp",
        "--launch",
        "--mgmt",
        "--connect-timeout",
        // A one-off preset pick, same grammar the session documents (`--preset ""` forces the
        // global defaults). `--profile` is its pre-rename spelling, which scripts and Decky
        // wrappers still pass.
        "--preset",
        "--profile",
    ];
    let mut cmd = std::process::Command::new(crate::app::spawn::session_binary());
    let mut args = std::env::args().skip(1).peekable();
    while let Some(a) = args.next() {
        if a == "--fullscreen" || a == "--stats" {
            cmd.arg(a);
        } else if forward.contains(&a.as_str()) {
            cmd.arg(&a);
            if let Some(v) = args.peek() {
                if !v.starts_with("--") {
                    cmd.arg(args.next().unwrap());
                }
            }
        }
    }
    if !arg_flag("--browse") && arg_value("--connect").is_none() {
        cmd.arg("--browse");
    }
    let err = cmd.exec(); // only returns on failure
    eprintln!("exec punktfunk-session: {err}");
    glib::ExitCode::FAILURE
}

/// `--library host[:mgmt_port]` — fetch and print the host's game library over the real
/// mTLS + pinned-fingerprint client, no GTK window. The pin comes from `--fp HEX` when given,
/// else the saved record for that address. Without one this refuses: an absent pin is not a
/// weaker check, it is no check — the verifier accepts any certificate.
pub fn headless_library(target: &str) -> glib::ExitCode {
    // `parse_addr_port` defaults a bare host to the STREAM port, but here a bare host means
    // "ask the saved record" — so a port counts only when the target spelled one out. A bare
    // `::1` carries colons and no port, which is why this is not a colon test.
    let (addr, explicit_port) = match pf_client_core::deeplink::parse_addr_port(target) {
        Some((a, p)) => {
            let spelled = target.len() > a.len() && target.ends_with(&format!(":{p}"));
            (a, spelled.then_some(p))
        }
        None => (target.to_string(), None),
    };
    let identity = match crate::trust::load_or_create_identity() {
        Ok(i) => i,
        Err(e) => {
            eprintln!("client identity: {e:#}");
            return glib::ExitCode::FAILURE;
        }
    };
    // The saved record is keyed by its STREAM port, so it is resolved by address alone — a
    // lookup on the mgmt port never matches, and `fetch_games` reads a `None` pin as "accept
    // any certificate". A host we cannot pin is refused rather than fetched unpinned.
    let known = crate::trust::KnownHosts::load();
    let saved = known
        .hosts
        .iter()
        .find(|h| h.addr == addr && !h.fp_hex.is_empty());
    let Some(pin) = arg_value("--fp")
        .as_deref()
        .and_then(crate::trust::parse_hex32)
        .or_else(|| saved.and_then(|h| crate::trust::parse_hex32(&h.fp_hex)))
    else {
        eprintln!("library: no pinned fingerprint for {addr} — pair the host first, or pass --fp");
        return glib::ExitCode::FAILURE;
    };
    // An explicit `host:port` names the mgmt port; otherwise the saved record's own.
    let port = explicit_port
        .or_else(|| saved.map(|h| h.effective_mgmt_port()))
        .unwrap_or(pf_client_core::library::DEFAULT_MGMT_PORT);
    match pf_client_core::library::fetch_games(&addr, port, &identity, Some(pin)) {
        Ok(games) => {
            // A fourth column, appended: `game` or `launcher` (design D4). Appended rather than
            // folded into an existing field so anything reading the first three columns is
            // untouched.
            for g in &games {
                let role = if g.is_launcher() { "launcher" } else { "game" };
                println!("{}\t{}\t{}\t{}", g.id, g.store, g.title, role);
            }
            let launchers = games.iter().filter(|g| g.is_launcher()).count();
            match launchers {
                0 => println!("{} game(s)", games.len()),
                n => println!("{} game(s), {} launcher(s)", games.len() - n, n),
            }
            glib::ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("library: {e}");
            glib::ExitCode::FAILURE
        }
    }
}

// `client-known-hosts.json` is the one host store every client on this device reads — the GTK
// shell, the Vulkan session, and the Decky plugin, which shells out to these headless modes. So
// `--set-host` and `--forget-host` mutate exactly that store, and an edit made in one surface
// shows up in the others.

/// Selector for `--set-host`/`--forget-host`: a 64-hex fingerprint pins one entry across IP
/// changes; anything else is treated as `addr[:port]` (manual entries have no fingerprint).
enum Selector {
    Fp(String),
    Addr(String, u16),
}

fn parse_selector(s: &str) -> Selector {
    if s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()) {
        Selector::Fp(s.to_lowercase())
    } else {
        let (addr, port) = parse_host_port(s);
        Selector::Addr(addr, port.unwrap_or(9777))
    }
}

impl Selector {
    fn matches(&self, h: &KnownHost) -> bool {
        match self {
            Selector::Fp(fp) => h.fp_hex.eq_ignore_ascii_case(fp),
            Selector::Addr(addr, port) => h.addr == *addr && h.port == *port,
        }
    }
}

/// `--omarchy-menu on|off|sync` — Punktfunk's rows in the Omarchy menu (Super+Space): a root
/// submenu, open/console rows, and one connect row per saved host, kept in sync by every
/// binary that changes the store. `on` is the consent step (mirroring `punktfunk-omarchy
/// setup` on the host side); `off` removes exactly our block; `sync` rewrites it now.
fn headless_omarchy_menu(verb: &str) -> glib::ExitCode {
    use pf_client_core::omarchy_menu as menu;
    let done = |msg: &str| {
        println!("{msg}");
        glib::ExitCode::SUCCESS
    };
    let failed = |e: String| {
        eprintln!("omarchy-menu: {e}");
        glib::ExitCode::FAILURE
    };
    match verb {
        "on" => match menu::enable() {
            Ok(()) => done("Punktfunk added to the Omarchy menu (Super+Space) — it follows your saved hosts from here"),
            Err(e) => failed(e),
        },
        "off" => match menu::disable() {
            Ok(()) => done("Punktfunk removed from the Omarchy menu"),
            Err(e) => failed(e),
        },
        "sync" => {
            if !menu::enabled() {
                return done("not enabled — run `punktfunk-client --omarchy-menu on` first");
            }
            match menu::enable() {
                Ok(()) => done("menu rows rewritten from the saved hosts"),
                Err(e) => failed(e),
            }
        }
        other => failed(format!("unknown verb {other:?} — use on, off or sync")),
    }
}

/// `--set-host <fp|host[:port]> [--host-label NAME] [--addr ADDR] [--port PORT] [--mac LIST]` —
/// edit a saved host: rename it, re-point its address (remembering the one it leaves), or set
/// its Wake-on-LAN MACs (`--mac ""` clears them). Identified by fingerprint (survives IP
/// changes) or current address. Prints `updated <name>`; fails if nothing matched or a value
/// doesn't parse.
pub fn headless_set_host(selector: &str) -> glib::ExitCode {
    let fail = |msg: String| {
        eprintln!("set-host: {msg}");
        glib::ExitCode::FAILURE
    };
    let port = match arg_value("--port") {
        None => None,
        Some(p) => match p.trim().parse::<u16>().ok().filter(|&p| p != 0) {
            Some(p) => Some(p),
            None => return fail(format!("invalid port {p:?}")),
        },
    };
    let macs = match arg_value("--mac").map(|m| pf_client_core::wol::parse_mac_list(&m)) {
        None => None,
        Some(Ok(macs)) => Some(macs),
        Some(Err(bad)) => return fail(format!("invalid MAC address {bad:?}")),
    };
    let edit = crate::trust::HostEdit {
        name: arg_value("--host-label"),
        addr: arg_value("--addr"),
        port,
        macs,
    };
    let sel = parse_selector(selector);
    let mut known = KnownHosts::load();
    let Some(h) = known.hosts.iter_mut().find(|h| sel.matches(h)) else {
        return fail(format!("no saved host matches {selector:?}"));
    };
    h.apply_edit(&edit);
    let label = h.name.clone();
    match known.save() {
        Ok(()) => {
            println!("updated {label}");
            glib::ExitCode::SUCCESS
        }
        Err(e) => fail(format!("{e:#}")),
    }
}

/// `--forget-host <fp|host[:port]>` — remove a saved host, its cached catalog and a default
/// pointing at it (a later connect must re-pair/trust). Prints `forgot N`; succeeds even if
/// nothing matched (idempotent).
///
/// An address naming two PINNED records is refused: both OS installs of a dual-boot box answer
/// at one lease, and forgetting a host is not undoable. The fingerprint selects one of them.
pub fn headless_forget_host(selector: &str) -> glib::ExitCode {
    let sel = parse_selector(selector);
    let mut known = KnownHosts::load();
    if let Selector::Addr(addr, port) = &sel {
        let pinned: Vec<&KnownHost> = known
            .hosts
            .iter()
            .filter(|h| h.addr == *addr && h.port == *port && !h.fp_hex.is_empty())
            .collect();
        if pinned.len() > 1 {
            eprintln!(
                "Couldn't forget {addr}:{port} — it names {} saved hosts. \
                 Forget one by its fingerprint:",
                pinned.len()
            );
            for h in pinned {
                eprintln!("  {}  {}", h.fp_hex, h.name);
            }
            return glib::ExitCode::FAILURE;
        }
    }
    let hits: Vec<usize> = (0..known.hosts.len())
        .filter(|&i| sel.matches(&known.hosts[i]))
        .collect();
    for &i in hits.iter().rev() {
        if let Err(e) = pf_client_core::orchestrate::forget_host(&mut known, i) {
            eprintln!("forget-host: {e:#}");
            return glib::ExitCode::FAILURE;
        }
    }
    println!("forgot {}", hits.len());
    glib::ExitCode::SUCCESS
}

/// The version this binary was built as — the CI-stamped string (`0.23.0~ci10250.gab12cd34`
/// and friends) when built by a packaging job, the crate version otherwise. See `build.rs`.
pub fn version_string() -> &'static str {
    env!("PUNKTFUNK_VERSION")
}

/// `--version` — one line, so a packaging script's run-the-binary gate and the Decky plugin
/// can both ask "what is installed here?" without a display or a config dir.
fn headless_version() -> glib::ExitCode {
    println!("punktfunk-client {}", version_string());
    glib::ExitCode::SUCCESS
}

/// `--check-update [--json]` — is a newer client available for this box's channel, and can
/// anything here install it? The answer comes from the same Ed25519-signed manifest the host
/// checks (`pf_client_core::update`); this is only its presentation.
///
/// Exit code carries the verdict for scripts: 0 = up to date, 10 = an update is available,
/// 1 = the check could not be completed (offline, bad signature, disabled). The distinction
/// matters — "could not tell" must never be scripted as "up to date".
fn headless_check_update() -> glib::ExitCode {
    let status = pf_client_core::update::check(version_string());
    if arg_flag("--json") {
        match serde_json::to_string(&status) {
            Ok(s) => println!("{s}"),
            Err(e) => {
                eprintln!("check-update: {e}");
                return glib::ExitCode::FAILURE;
            }
        }
    } else {
        println!(
            "installed  {} ({}, {})",
            status.current, status.kind, status.channel
        );
        // `latest` falls back to `current` when the check couldn't run — printing that as
        // "available" would read as a confirmed answer we don't have.
        if status.error.is_some() {
            println!("available  unknown");
        } else {
            println!("available  {}", status.latest);
        }
        if status.not_published {
            // Says what it is, in words, instead of a raw HTTP status. The exit code still
            // reports "could not tell" (see the doc comment above): an empty channel is the
            // absence of evidence that this build is current, and a mistyped
            // PUNKTFUNK_UPDATE_FEED is indistinguishable from one out here.
            println!(
                "update     nothing published on the {} channel yet",
                status.channel
            );
        } else if let Some(err) = &status.error {
            eprintln!("check-update: {err}");
        } else if status.update_available {
            println!("update     yes");
            println!("apply      {}", update_apply_line(&status));
        } else {
            println!("update     no");
        }
        if let Some(hint) = &status.opt_in_hint {
            println!("opt-in     {hint}");
        }
    }
    if status.error.is_some() {
        glib::ExitCode::FAILURE
    } else if status.update_available {
        // Distinct from both success and failure so `--check-update` is scriptable.
        glib::ExitCode::from(10u8)
    } else {
        glib::ExitCode::SUCCESS
    }
}

/// The human line describing how an update would be applied.
fn update_apply_line(status: &pf_client_core::update::Status) -> String {
    use pf_client_core::update::Applier;
    match status.applier {
        Applier::Helper => format!("punktfunk-client --apply-update   ({})", status.command),
        Applier::Flatpak => status.command.clone(),
        Applier::None => status.command.clone(),
    }
}

/// `--apply-update [--json]` — drive the packaged root helper for this install. Refuses (with
/// the command to run instead) for every kind it does not own; see
/// `pf_client_core::update::apply`.
fn headless_apply_update() -> glib::ExitCode {
    let outcome = pf_client_core::update::apply(version_string());
    if arg_flag("--json") {
        match serde_json::to_string(&outcome) {
            Ok(s) => println!("{s}"),
            Err(e) => {
                eprintln!("apply-update: {e}");
                return glib::ExitCode::FAILURE;
            }
        }
    } else if let Some(err) = &outcome.error {
        eprintln!("apply-update: {err}");
    } else if outcome.staged {
        println!(
            "staged {} -> {} (reboot to finish)",
            outcome.before, outcome.after
        );
    } else if outcome.changed {
        println!("updated {} -> {}", outcome.before, outcome.after);
    } else {
        println!("already up to date ({})", outcome.before);
    }
    if outcome.ok {
        glib::ExitCode::SUCCESS
    } else {
        glib::ExitCode::FAILURE
    }
}

/// Dispatch the headless host-store modes (returns `None` when argv names none of them, so the
/// caller proceeds to launch the GTK app). Kept in one place so `arg_flag` stays private and the
/// dispatch in `app.rs` is a single line.
pub fn headless_host_command() -> Option<glib::ExitCode> {
    if let Some(v) = arg_value("--omarchy-menu") {
        return Some(headless_omarchy_menu(&v));
    }
    if let Some(s) = arg_value("--set-host") {
        return Some(headless_set_host(&s));
    }
    if let Some(s) = arg_value("--forget-host") {
        return Some(headless_forget_host(&s));
    }
    if arg_flag("--version") {
        return Some(headless_version());
    }
    if arg_flag("--check-update") {
        return Some(headless_check_update());
    }
    if arg_flag("--apply-update") {
        return Some(headless_apply_update());
    }
    None
}
