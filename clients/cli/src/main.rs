//! `punktfunk` — the headless client CLI (design/client-architecture-split.md §4).
//!
//! One console-subsystem binary over the same brain the GUI shells use, so a script gets the
//! same behaviour a click does — including wake-then-connect, which the Linux shell's old
//! exec-style `--connect` never had. It is a FRONT-END, not the brain: policy lives in
//! `pf_client_core`, and the shells call the same functions in-process rather than shelling
//! out to this. That is the whole point of the split — if the GUI shelled out for connects,
//! trust prompts and wake progress would have to squeeze through an IPC contract.
//!
//! Existing surfaces are a frozen compatibility contract and are NOT replaced by this: the
//! Linux shell keeps its headless flags (Decky invokes them), and `punktfunk-probe` stays the
//! diagnostics tool. This is the door new consumers should use — the Playnite importer shells
//! to `punktfunk library <host> --json`.
//!
//! Exit codes extend the session binary's: 0 ok, 2 connect failed, 3 trust rejected,
//! 4 renderer failed, 5 could not resolve what was asked for, 6 refused because it needs a
//! human (pairing, an unknown host). A machine consumer can branch on those without parsing
//! prose.

#![forbid(unsafe_code)]

#[cfg(any(target_os = "linux", windows))]
mod cli {
    use pf_client_core::deeplink::{self, DeepLink, HostResolution};
    use pf_client_core::orchestrate::{
        self, ConnectOutcome, ConnectPlan, PlanOutcome, SessionEvent, WakeOutcome, WakeTick,
        WAKE_TIMEOUT_SECS,
    };
    use pf_client_core::presets::PresetsFile;
    use pf_client_core::trust::{self, KnownHost, KnownHosts, Settings};
    use pf_client_core::{library, start, wol};
    use std::time::Duration;

    pub const OK: u8 = 0;
    // The session's own codes pass through; 5 and 6 are the CLI's.
    pub use orchestrate::exit::{CONNECT_FAILED, RENDERER_FAILED, TRUST_REJECTED};
    /// Nothing here matches what you named (host, preset, game).
    pub const UNRESOLVED: u8 = 5;
    /// Refused because it needs a person: pairing, or trusting an unknown host.
    pub const NEEDS_INTERACTION: u8 = 6;

    const PROBE_TIMEOUT: Duration = Duration::from_millis(2500);

    /// The handshake budget `--request-access` runs on. Matches the host's `PENDING_APPROVAL_WAIT`
    /// — the connect is PARKED for that long while an operator decides, so anything shorter would
    /// give up while the approval prompt is still on their screen.
    const REQUEST_ACCESS_TIMEOUT_SECS: u64 = 185;

    const USAGE: &str = "\
punktfunk — the Punktfunk client, headless

  punktfunk discover [--json] [--timeout SECS]
  punktfunk pair <host[:port]> [--pin -] [--name LABEL]
  punktfunk hosts list [--probe] [--json]
  punktfunk hosts add <host[:port]> [--name LABEL] [--fp HEX]
  punktfunk hosts forget <host-ref>
  punktfunk default-host [<host-ref>] [--clear]
  punktfunk wake <host-ref> [--wait]
  punktfunk library [<host-ref>] [--json]
  punktfunk end-game [<host-ref>] --game ID
  punktfunk launch [<host-ref>] [--game ID] [--preset REF] [--request-access]
                                [--exec] [--fullscreen]
  punktfunk open <punktfunk://…> [--yes]
  punktfunk reachable <host-ref>
  punktfunk speed-test <host-ref>
  punktfunk presets list [--json]
  punktfunk reset

A <host-ref> is a saved host's id, its name, or an address — the same reference a
punktfunk:// link takes. Where it is optional, leaving it out means the default host.
Exit codes: 0 ok, 2 connect, 3 trust, 4 renderer, 5 not found, 6 needs a person.

\"punktfunk help <command>\" (or any command with --help) explains that command.";

    /// The long help for one verb — `punktfunk help <verb>`, or `--help` after the verb.
    /// Each entry documents its flags and the behaviour a script would need to know
    /// (what goes to stdout vs stderr, and which exit codes mean what).
    fn verb_help(verb: &str) -> Option<&'static str> {
        Some(match verb {
            "discover" => {
                "\
punktfunk discover [--json] [--timeout SECS] — browse the LAN for hosts

Listens for Punktfunk hosts advertising over mDNS and prints what answered:
name TAB addr:port TAB saved|new TAB paired|unpaired. `saved` means this
device already has a record for it, matched by fingerprint first and address
second — the same rule every other surface joins the two lists by.

  --timeout SECS  how long to browse (default 3, capped at 30) — a bounded
                  call, so a panel can wait for it
  --json          {\"hosts\":[{\"name\",\"addr\",\"port\",\"fp\",\"pair\",\"id\",\"mgmt\",
                  \"os\",\"saved\",\"paired\"}]}

Nothing answering is an answer, not a failure: an empty list exits 0. A host
mDNS never sees (Tailscale, another subnet) will not appear here — save it by
address with `punktfunk hosts add` and it shows in `hosts list --probe`."
            }
            "pair" => {
                "\
punktfunk pair <host[:port]> — enrol this device with a host (PIN ceremony)

  --pin -       read the PIN from stdin (one line). Without it the command asks,
                and refuses (exit 6) when there is no terminal to ask on
  --name LABEL  the label the host files this device under
                (default: this machine's name)

Pairing verifies the host end-to-end and pins its fingerprint in the saved-hosts
store, so every later connect — here, in the desktop client or the console — is
silent. The port defaults to 9777. Prints `paired <addr>:<port> fp=<hex>` on
success; exit 3 if the host refuses or the PIN is wrong."
            }
            "hosts" => {
                "\
punktfunk hosts — the saved-hosts store (shared with the desktop client)

  punktfunk hosts list [--probe] [--json]
      Every saved host, name TAB addr:port TAB paired/trusted TAB state.
      --probe asks each host directly (no mDNS, so routed/VPN hosts answer
      too); --json emits one object with per-host detail, presets included.

  punktfunk hosts add <host[:port]> [--name LABEL] [--fp HEX]
      Save a host by address — the door for a box mDNS never sees (Tailscale,
      another subnet). Without --fp it is a placeholder to pair later; with a
      64-hex fingerprint it is pinned immediately (still unpaired).

      Idempotent, and keyed on the FINGERPRINT once there is one: re-running it
      for a host already saved is a no-op, and giving a known fingerprint a new
      address MOVES that host's record there rather than filing a second one
      (which is how a host that changed DHCP lease stays reachable by its id).
      A different fingerprint for an address already saved is refused, exit 3 —
      a changed identity is a decision for a person.

  punktfunk hosts forget <host-ref>
      Remove a saved host with its pinned fingerprint and cached game list,
      and stop opening on it by default. A later connect must pair or trust
      it again."
            }
            "default-host" => {
                "\
punktfunk default-host [<host-ref>] [--clear] — the host a bare launch opens on

Prints `name TAB addr:port TAB explicit|derived`, or `none`. With a <host-ref> it
points the setting at that host and prints the record it wrote; with --clear it
unsets it.

The pointer is only half the answer: with exactly one paired host saved, that host
is the default with nothing written here, which is what `derived` means. Naming one
explicitly matters once a second host is paired — a derived default is dropped then,
an explicit one survives. An unpaired host is refused (exit 6): a launch cannot pair,
so the setting would never resolve.

Every client reads this: `punktfunk launch` and `library` with no <host-ref>, and the
console, desktop and mobile shells at startup, subject to their Start in setting."
            }
            "wake" => {
                "\
punktfunk wake <host-ref> [--wait] — Wake-on-LAN

Sends a magic packet to a saved host's MAC (learned from its advert while it
was awake; exit 5 if none is known yet). With --wait, keeps sending every 6 s
and polls presence every second for up to 90 s, exiting 0 the moment the host
answers — the same cadence every graphical shell uses."
            }
            "library" => {
                "\
punktfunk library [<host-ref>] [--json] — the host's game library

TSV on stdout by default (id TAB store TAB title), one game per line; --json
emits {\"games\":[…]} for tools — the Playnite importer shells to exactly
this. Needs a paired host (exit 6 otherwise).

With no <host-ref> it asks the default host (`punktfunk default-host`), and
exits 5 when there is none."
            }
            "end-game" => {
                "\
punktfunk end-game [<host-ref>] --game ID — end a title this device launched

Asks the host to close the game, live stream included. The host ends only a
title this device launched; one another device started stays up. Prints the
outcome on stdout.

Exit 0 when it ended, 5 when the host had nothing of it running, 3 when this
device's access expired, 2 when the host couldn't be asked or is too old,
6 when the host isn't paired."
            }
            "launch" => {
                "\
punktfunk launch [<host-ref>] [--game ID] [--preset REF] [--request-access]
                              [--exec] [--fullscreen]

Start a stream — waking the host first if it is asleep and its MAC is known.
The stream runs in the punktfunk-session renderer; this command supervises it
and relays its lifecycle to stderr. With no <host-ref> it streams the default
host (`punktfunk default-host`), and exits 5 when there is none.

  --game ID      ask the host to launch this library title into the stream
  --preset REF   use a preset (id or name) for this connect only;
                 without it the host's own binding applies
  --fullscreen   start the stream window fullscreen
  --exec         become the session process instead of supervising it — the
                 gamescope-wrapper mode, where the launched process must BE
                 the streaming one for focus and lifecycle to work
  --request-access
                 ask the host's operator to let this device in instead of
                 typing a PIN. The host PARKS the connect until somebody
                 approves it in its console or web UI (up to ~185 s), then
                 admits it and the stream starts by itself; the host is
                 recorded as paired once that happens, so later streams are
                 silent. Needs the host's fingerprint pinned already
                 (`punktfunk hosts add <addr> --fp <hex>`), and cannot be
                 combined with --exec — under --exec there is no process
                 left to record the approval.

Exit 0 when the stream ends cleanly, 2 connect failed, 3 the host no longer
trusts this device (re-pair), 4 the renderer could not start."
            }
            "open" => {
                "\
punktfunk open <punktfunk://…> — follow a punktfunk:// link, headless

Same parser and same refusal rules as clicking the link in a shell: a
contradicted fingerprint refuses and says so, an ambiguous name refuses
rather than guessing, and an unknown host is never trusted from a URL —
that is a decision for a person, at a surface that can show the fingerprint
(exit 6 points at `punktfunk pair`). --exec as in launch.

A link that names its host by the stable record id opens straight away. One
that names it by label or address is a guess anything could make, so it asks
first; --yes answers for a script, and without a terminal it refuses (exit 6)
rather than opening unasked."
            }
            "reachable" => {
                "\
punktfunk reachable <host-ref> — one bounded reachability probe

Asks the host directly (no mDNS), so routed/VPN hosts answer too. The
reference may be an unsaved address — this verb answers \"can I reach it\",
not \"do I know it\". Exit 0 reachable, 2 not; one line either way."
            }
            "network-check" => {
                "\
punktfunk network-check <host-ref> [--json] — what the path to a host does

Opens a diagnostic session that streams nothing, runs the speed test, two
bursty legs at the clean round's rate and a slow round, reads both ends'
network facts, and names what it found: a faster host port, a receiver that
drops line-rate bursts, a small receive buffer, a link fault, a queue, Wi-Fi.
Each finding says which delivery profile helps, if one does. Exit 0 when the
check ran, whatever it found."
            }
            "speed-test" => {
                "\
punktfunk speed-test <host-ref> [--json] — measure the real data plane

Runs the host's bandwidth probe over an actual session connect and prints the
measured throughput, loss, and the bitrate it recommends. Deliberately does
NOT apply the result: which layer a bitrate belongs in (a bound preset, the
global default) is a decision the GUI makes with the user, and a CLI silently
rewriting settings would be exactly the surprise that rule exists to prevent."
            }
            "presets" | "profiles" => {
                "\
punktfunk presets list [--json] — the stream presets on this device

One line per preset: id TAB name TAB how many settings it overrides.
Presets are created and edited in the desktop client; a connect uses one via
`punktfunk launch --preset` or a punktfunk:// link that names it.
`punktfunk profiles` is the old spelling; its --json keeps the `profiles` key."
            }
            "reset" => {
                "\
punktfunk reset — forget every saved host and reset stream settings

Asks for confirmation, and refuses (exit 6) when there is no terminal to ask
on. This device's identity keypair is deliberately kept, so hosts that knew
this machine still recognise it after re-pairing; delete the identity files
from the config directory for a true factory reset."
            }
            _ => return None,
        })
    }

    /// The value after `--flag`, if any.
    fn value(args: &[String], flag: &str) -> Option<String> {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1))
            .filter(|v| !v.starts_with("--"))
            .cloned()
    }

    fn has(args: &[String], flag: &str) -> bool {
        args.iter().any(|a| a == flag)
    }

    /// The first argument that isn't a flag or a flag's value — the verb's subject.
    fn positional(args: &[String], skip: usize) -> Option<String> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < args.len() {
            if args[i].starts_with("--") {
                // Skip the flag and, when it takes one, its value.
                if args
                    .get(i + 1)
                    .is_some_and(|v| !v.starts_with("--") && flag_takes_value(&args[i]))
                {
                    i += 1;
                }
            } else {
                out.push(args[i].clone());
            }
            i += 1;
        }
        out.get(skip).cloned()
    }

    fn flag_takes_value(flag: &str) -> bool {
        matches!(
            flag,
            "--pin"
                | "--name"
                | "--fp"
                | "--game"
                | "--preset"
                | "--profile"
                | "--port"
                | "--timeout"
        )
    }

    /// Resolve a host reference the way every other surface does: stable id, then a unique
    /// name, then `addr[:port]` (design/client-deep-links.md §2). Sharing `resolve_host` is
    /// what keeps `punktfunk launch desk` and `punktfunk://connect/desk` from disagreeing.
    ///
    /// [`HostResolution::Confirm`] — a guessable reference — is accepted WITHOUT a prompt here,
    /// and only here: this reference is an argument the user typed in their own terminal, so
    /// there is nobody else to confirm it with. The guessable-reference rule exists for URLs
    /// handed to us by someone else; that path is `open`, which does ask.
    fn resolve(reference: &str) -> Result<(KnownHosts, usize), u8> {
        let known = KnownHosts::load();
        let link = DeepLink {
            host_ref: reference.to_string(),
            ..Default::default()
        };
        match deeplink::resolve_host(&link, &known) {
            HostResolution::Known(i) | HostResolution::Confirm(i) => Ok((known, i)),
            HostResolution::Ambiguous => {
                eprintln!(
                    "more than one saved host is called \"{reference}\" — use its address or id"
                );
                Err(UNRESOLVED)
            }
            HostResolution::Unknown { addr, port, .. } => {
                eprintln!(
                    "{addr}:{port} isn't a saved host — pair it first (punktfunk pair {addr}:{port})"
                );
                Err(NEEDS_INTERACTION)
            }
            HostResolution::Unresolvable => {
                eprintln!("no saved host matches \"{reference}\"");
                Err(UNRESOLVED)
            }
        }
    }

    /// The host a verb acts on: the `<host-ref>` it was given, else the default host.
    /// Omitting the reference is how a script on a one-host box stops naming which box.
    fn resolve_or_default(args: &[String], usage: &str) -> Result<(KnownHosts, usize), u8> {
        if let Some(reference) = positional(args, 0) {
            return resolve(&reference);
        }
        let known = KnownHosts::load();
        match start::default_host(&Settings::load(), &known) {
            Some(i) => Ok((known, i)),
            None => {
                eprintln!("usage: {usage}");
                eprintln!("no default host is set — see `punktfunk default-host`");
                Err(UNRESOLVED)
            }
        }
    }

    /// Read, set, or clear `Settings::default_host` — the pointer every client's bare
    /// launch resolves through. A headless box (a Deck over ssh) has no other door to it.
    fn default_host_cmd(args: &[String]) -> u8 {
        let mut settings = Settings::load();
        if has(args, "--clear") {
            settings.default_host = None;
            settings.save();
            println!("cleared");
            return OK;
        }
        if let Some(reference) = positional(args, 0) {
            let (known, i) = match resolve(&reference) {
                Ok(v) => v,
                Err(code) => return code,
            };
            let host = &known.hosts[i];
            // The resolver ignores an unpaired record, so writing one here would set a
            // pointer that never resolves — refuse rather than lie about it.
            if !host.paired || host.fp_hex.is_empty() {
                eprintln!("{} isn't paired — pair it first", host.name);
                return NEEDS_INTERACTION;
            }
            let Some(id) = host.id.clone() else {
                eprintln!("{} has no record id — re-save it", host.name);
                return UNRESOLVED;
            };
            settings.default_host = Some(id);
            settings.save();
            println!("{}\t{}:{}", host.name, host.addr, host.port);
            return OK;
        }
        let known = KnownHosts::load();
        match start::default_host_with_source(&settings, &known) {
            (Some(i), source) => {
                let h = &known.hosts[i];
                println!("{}\t{}:{}\t{}", h.name, h.addr, h.port, source.as_str());
                OK
            }
            (None, _) => {
                println!("none");
                OK
            }
        }
    }

    pub fn run(args: Vec<String>) -> u8 {
        let Some(verb) = args.first().cloned() else {
            println!("{USAGE}");
            return OK;
        };
        let rest: Vec<String> = args[1..].to_vec();
        // `--help`/`-h` after any verb prints that verb's help — before dispatch, so a verb
        // never mistakes the flag for its subject (`punktfunk pair -h` must not dial "-h").
        if rest.iter().any(|a| a == "--help" || a == "-h") {
            println!("{}", verb_help(&verb).unwrap_or(USAGE));
            return OK;
        }
        match verb.as_str() {
            "discover" => discover(&rest),
            "pair" => pair(&rest),
            "hosts" => hosts(&rest),
            "default-host" => default_host_cmd(&rest),
            "wake" => wake(&rest),
            "library" => library_cmd(&rest),
            "end-game" => end_game_cmd(&rest),
            "launch" => launch(&rest),
            "open" => open(&rest),
            "reachable" => reachable(&rest),
            "speed-test" => speed_test(&rest),
            "network-check" => network_check(&rest),
            "presets" => presets(&rest, false),
            "profiles" => presets(&rest, true),
            "reset" => reset(),
            "-h" | "--help" | "help" => match positional(&rest, 0) {
                None => {
                    println!("{USAGE}");
                    OK
                }
                Some(topic) => match verb_help(&topic) {
                    Some(h) => {
                        println!("{h}");
                        OK
                    }
                    None => {
                        eprintln!("no command called \"{topic}\"\n\n{USAGE}");
                        UNRESOLVED
                    }
                },
            },
            "--version" | "version" => {
                println!("punktfunk {}", env!("CARGO_PKG_VERSION"));
                OK
            }
            other => {
                eprintln!("unknown command \"{other}\"\n\n{USAGE}");
                UNRESOLVED
            }
        }
    }

    /// How long `discover` browses when nobody says, and the ceiling on what they can ask for.
    /// The cap is not politeness: this verb is called from a Quick Access panel, and a typo'd
    /// `--timeout 3000` would hang that panel with no way to cancel it.
    const DISCOVER_DEFAULT_SECS: f64 = 3.0;
    const DISCOVER_MAX_SECS: f64 = 30.0;

    /// `discover [--json] [--timeout SECS]` — browse the LAN over mDNS and print what answered,
    /// annotated against the saved-hosts store.
    ///
    /// The annotation is the point: a caller wants "can I stream this", which is a question
    /// about BOTH lists, and joining them itself is how two surfaces end up disagreeing about
    /// the same host. So the match rule lives here, once, and is the same one every other
    /// surface uses — fingerprint first (survives a DHCP move), address second.
    fn discover(args: &[String]) -> u8 {
        let secs = value(args, "--timeout")
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|s| *s > 0.0)
            .unwrap_or(DISCOVER_DEFAULT_SECS)
            .min(DISCOVER_MAX_SECS);
        let found = pf_client_core::discovery::discover_for(Duration::from_secs_f64(secs));
        // `read`, not `load`: this verb never hands a record's id back, so it has no business
        // MINTING one. `load` would mint ids for a pre-mint store and save them, racing the
        // `hosts list` a caller is very likely running at the same moment (the Decky panel issues
        // both together) — after which the ids one of them already handed out no longer resolve.
        let known = KnownHosts::read();
        let rows: Vec<(
            &pf_client_core::discovery::DiscoveredHost,
            Option<&KnownHost>,
        )> = found.iter().map(|d| (d, match_saved(&known, d))).collect();
        // The one write this verb does make, and why it doesn't contradict the above: an advert
        // is the only place a host's wake MAC is ever published, and this verb is the only one
        // the Decky panel runs that ever sees one. Without it a Deck in Gaming Mode never learns
        // a MAC at all and Wake-on-LAN cannot fire, with nothing to show for it (#322).
        // `learn_from_advert` mints nothing either, and writes only when an advert genuinely
        // taught the record something new — so a steady-state panel refresh touches no disk.
        for (d, saved) in &rows {
            if let Some(k) = saved {
                trust::learn_from_advert(
                    &k.fp_hex,
                    &k.addr,
                    k.port,
                    &d.addr,
                    &d.mac,
                    &d.os,
                    d.mgmt_port,
                );
            }
        }
        if has(args, "--json") {
            let hosts: Vec<serde_json::Value> = rows
                .iter()
                .map(|(d, saved)| {
                    serde_json::json!({
                        "name": d.name,
                        "addr": d.addr,
                        "port": d.port,
                        "fp": d.fp_hex,
                        "pair": d.pair,
                        "id": d.advertised_id(),
                        // 0 = not advertised, which is what a consumer's own "no mgmt port"
                        // already means — an older host simply omits the TXT.
                        "mgmt": d.mgmt_port.unwrap_or(0),
                        "os": d.os,
                        "saved": saved.is_some(),
                        // The record's stable id, so a consumer pairs the advert with the row
                        // it already listed instead of re-deriving the match. `null` when the
                        // record predates ids: this verb reads the store and never mints.
                        "saved_id": saved.and_then(|h| h.id.as_deref()),
                        "paired": saved.is_some_and(|h| h.paired),
                    })
                })
                .collect();
            println!("{}", serde_json::json!({ "hosts": hosts }));
        } else {
            for (d, saved) in &rows {
                println!(
                    "{}\t{}:{}\t{}\t{}",
                    d.name,
                    d.addr,
                    d.port,
                    if saved.is_some() { "saved" } else { "new" },
                    if saved.is_some_and(|h| h.paired) {
                        "paired"
                    } else {
                        "unpaired"
                    },
                );
            }
        }
        // An empty LAN is an answer, not a failure — a caller branching on the exit code is
        // asking "did the browse run", and it did.
        OK
    }

    /// The saved record an advert belongs to, if any: an exact fingerprint first, then
    /// [`same_host`](pf_client_core::discovery::same_host).
    ///
    /// Two known fingerprints settle it on their own, so a host that moved lease still matches
    /// its record and a different box at the old address (the other OS of a dual-boot machine,
    /// or whoever inherited the lease) does not inherit its pairing. The plugin's `mergeHosts`
    /// and the shells' hosts pages use the same rule.
    fn match_saved<'a>(
        known: &'a KnownHosts,
        advert: &pf_client_core::discovery::DiscoveredHost,
    ) -> Option<&'a KnownHost> {
        known
            .hosts
            .iter()
            .find(|h| !h.fp_hex.is_empty() && h.fp_hex.eq_ignore_ascii_case(&advert.fp_hex))
            .or_else(|| {
                known
                    .hosts
                    .iter()
                    .find(|h| pf_client_core::discovery::same_host(h, advert))
            })
    }

    /// Run the SPAKE2 ceremony, prompting on a terminal or reading `--pin -` from stdin.
    /// Literal PIN arguments are refused because process command lines are public metadata.
    fn pair(args: &[String]) -> u8 {
        let Some(target) = positional(args, 0) else {
            eprintln!("usage: punktfunk pair <host[:port]> [--pin -]");
            return UNRESOLVED;
        };
        let (addr, port) = split_host_port(&target);
        let pin = match value(args, "--pin").as_deref() {
            Some("-") => read_pin(None),
            Some(_) => {
                eprintln!("a PIN may not be passed in argv; use --pin - or an interactive prompt");
                return NEEDS_INTERACTION;
            }
            None if is_tty() => read_pin(Some(&addr)),
            None => {
                eprintln!("no --pin - and no terminal to ask on");
                return NEEDS_INTERACTION;
            }
        };
        let Some(pin) = pin else {
            eprintln!("no PIN on stdin");
            return NEEDS_INTERACTION;
        };
        let identity = match trust::load_or_create_identity() {
            Ok(i) => i,
            Err(e) => {
                eprintln!("client identity: {e:#}");
                return CONNECT_FAILED;
            }
        };
        let name = value(args, "--name").unwrap_or_else(trust::device_name);
        match trust::pair_with_host(&addr, port, &identity, &pin, &name) {
            Ok(fp) => {
                let fp_hex = trust::hex(&fp);
                if let Err(e) = trust::persist_host(&addr, &addr, port, &fp_hex, true, &[]) {
                    eprintln!("couldn't save the host: {e:#}");
                }
                trust::forget_placeholder(&addr, port);
                println!("paired {addr}:{port} fp={fp_hex}");
                OK
            }
            Err(e) => {
                eprintln!("{}", trust::pair_error_message(&e));
                TRUST_REJECTED
            }
        }
    }

    /// `hosts list|add|forget` over the shared store — the same file the shells and the session
    /// read, so a change here shows up there.
    fn hosts(args: &[String]) -> u8 {
        match positional(args, 0).as_deref() {
            Some("list") | None => {
                let known = KnownHosts::load();
                let online: Option<Vec<bool>> = has(args, "--probe").then(|| {
                    trust::probe_reachable_many(
                        known
                            .hosts
                            .iter()
                            .map(|h| (h.addr.clone(), h.port, h.fp_hex.clone()))
                            .collect(),
                        PROBE_TIMEOUT,
                    )
                });
                if has(args, "--json") {
                    let catalog = PresetsFile::load();
                    let rows: Vec<serde_json::Value> = known
                        .hosts
                        .iter()
                        .enumerate()
                        .map(|(i, h)| {
                            let preset = h
                                .preset_id
                                .as_ref()
                                .and_then(|id| catalog.find_by_id(id))
                                .map(|p| serde_json::json!({"id": p.id, "name": p.name}));
                            let pinned: Vec<_> = h
                                .resolved_pins(&catalog)
                                .iter()
                                .map(|p| serde_json::json!({"id": p.id, "name": p.name}))
                                .collect();
                            // `profile` / `pinned_profiles` are the pre-rename keys, for a Decky
                            // plugin older than the rename.
                            serde_json::json!({
                                "id": h.id,
                                "name": h.name,
                                "addr": h.addr,
                                "port": h.port,
                                "fp_hex": h.fp_hex,
                                "paired": h.paired,
                                "mac": h.mac,
                                "os": h.os,
                                "last_used": h.last_used,
                                "clipboard_sync": h.clipboard_sync,
                                "preset": preset,
                                "pinned_presets": pinned,
                                "profile": preset,
                                "pinned_profiles": pinned,
                                "online": online.as_ref().map(|v| v[i]),
                            })
                        })
                        .collect();
                    println!("{}", serde_json::json!({ "hosts": rows }));
                } else {
                    for (i, h) in known.hosts.iter().enumerate() {
                        let state = match online.as_ref().map(|v| v[i]) {
                            Some(true) => "online",
                            Some(false) => "offline",
                            None => "-",
                        };
                        println!(
                            "{}\t{}:{}\t{}\t{state}",
                            h.name,
                            h.addr,
                            h.port,
                            if h.paired { "paired" } else { "trusted" }
                        );
                    }
                }
                OK
            }
            Some("add") => {
                let Some(target) = positional(args, 1) else {
                    eprintln!("usage: punktfunk hosts add <host[:port]> [--name LABEL] [--fp HEX]");
                    return UNRESOLVED;
                };
                let (addr, port) = split_host_port(&target);
                let fp = value(args, "--fp").unwrap_or_default();
                let name = value(args, "--name");
                let mut known = KnownHosts::load();
                if let Some(i) = add_target(&known, &addr, port, &fp) {
                    match merge_saved_host(&mut known, i, &fp, name.as_deref()) {
                        AddOutcome::Unchanged => {
                            eprintln!("{addr}:{port} is already saved");
                            return OK;
                        }
                        AddOutcome::Pinned => {
                            return match known.save() {
                                Ok(()) => {
                                    println!("updated {addr}:{port}");
                                    OK
                                }
                                Err(e) => {
                                    eprintln!("saving: {e:#}");
                                    CONNECT_FAILED
                                }
                            }
                        }
                        // The guard held: nothing was overwritten, so file this identity below.
                        AddOutcome::Conflict => {}
                    }
                }
                // Nothing to merge into here — but a record with this exact FINGERPRINT is this
                // same host at a new address. Re-point it: a moved host would otherwise file one
                // record per address it has ever held, and the one its stable id resolves to
                // keeps an address it has left.
                if let Some(i) = known
                    .hosts
                    .iter()
                    .position(|h| !fp.is_empty() && h.fp_hex.eq_ignore_ascii_case(&fp))
                {
                    let was = format!("{}:{}", known.hosts[i].addr, known.hosts[i].port);
                    known.hosts[i].move_to(&addr, port);
                    return match known.save() {
                        Ok(()) => {
                            println!("moved {was} to {addr}:{port}");
                            OK
                        }
                        Err(e) => {
                            eprintln!("saving: {e:#}");
                            CONNECT_FAILED
                        }
                    };
                }
                known.hosts.push(KnownHost {
                    name: name.unwrap_or_else(|| addr.clone()),
                    addr: addr.clone(),
                    port,
                    fp_hex: fp,
                    ..Default::default()
                });
                match known.save() {
                    Ok(()) => {
                        println!("added {addr}:{port}");
                        OK
                    }
                    Err(e) => {
                        eprintln!("saving: {e:#}");
                        CONNECT_FAILED
                    }
                }
            }
            Some("forget") => {
                let Some(reference) = positional(args, 1) else {
                    eprintln!("usage: punktfunk hosts forget <host-ref>");
                    return UNRESOLVED;
                };
                let (mut known, i) = match resolve(&reference) {
                    Ok(v) => v,
                    Err(code) => return code,
                };
                match orchestrate::forget_host(&mut known, i) {
                    Ok(gone) => {
                        println!("forgot {}", gone.name);
                        OK
                    }
                    Err(e) => {
                        eprintln!("saving: {e:#}");
                        CONNECT_FAILED
                    }
                }
            }
            Some(other) => {
                eprintln!("unknown hosts command \"{other}\" — list, add or forget");
                UNRESOLVED
            }
        }
    }

    /// What `hosts add` did to a record that was ALREADY saved for this address.
    #[derive(Debug, PartialEq, Eq)]
    enum AddOutcome {
        /// Nothing to do — no fingerprint was offered, or the record already carries this one.
        /// Exits 0 on purpose: a panel retrying step 1 of request access must not have to
        /// invent an error to show for a state that is already correct.
        Unchanged,
        /// The record had no fingerprint and now has this one.
        Pinned,
        /// The record carries a DIFFERENT fingerprint. Never overwritten — a second identity
        /// is filed beside it.
        Conflict,
    }

    /// The saved record `hosts add` is ABOUT: the one at this address already carrying this
    /// fingerprint, or the unpinned placeholder waiting for one.
    ///
    /// A record there carrying a DIFFERENT fingerprint is a different host, and is not this
    /// add's target — a dual-boot box answers on one lease with one MAC and a certificate per
    /// OS, so the second one is filed beside the first rather than refused for its address.
    fn add_target(known: &KnownHosts, addr: &str, port: u16, fp: &str) -> Option<usize> {
        known.hosts.iter().position(|h| {
            h.addr == addr
                && h.port == port
                && (fp.is_empty() || h.fp_hex.is_empty() || h.fp_hex.eq_ignore_ascii_case(fp))
        })
    }

    /// `hosts add --fp` against an address that is already saved. The difference between these
    /// three is a trust decision, not bookkeeping.
    ///
    /// Filling in an empty fingerprint is step 1 of request access (design §5): a host found by
    /// advert is saved by address first and pinned second. Without it the `--fp` is dropped on
    /// the floor and the launch that follows refuses for want of a pin — which is what this did
    /// before, silently and with exit 0.
    ///
    /// A *different* fingerprint never lands on this record: a changed identity is a decision
    /// for a person, and quietly overwriting a pin here would be a back door through the
    /// pinning the rest of the client is built on. `hosts add` files it as its own record —
    /// see [`add_target`], which is why this outcome should not reach the caller.
    fn merge_saved_host(
        known: &mut KnownHosts,
        i: usize,
        fp: &str,
        name: Option<&str>,
    ) -> AddOutcome {
        let existing = known.hosts[i].fp_hex.clone();
        if fp.is_empty() || existing.eq_ignore_ascii_case(fp) {
            return AddOutcome::Unchanged;
        }
        if !existing.is_empty() {
            return AddOutcome::Conflict;
        }
        known.hosts[i].fp_hex = fp.to_string();
        // Only a record still named after its own address is renamed: a label the user chose is
        // theirs, and an advert's name must not quietly overwrite it.
        if let Some(label) = name {
            if known.hosts[i].name == known.hosts[i].addr {
                known.hosts[i].name = label.to_string();
            }
        }
        AddOutcome::Pinned
    }

    /// `wake <host-ref> [--wait]` — a magic packet, and with `--wait` the bounded wake-and-wait
    /// the console runs (`orchestrate::wake_by_probe`: a packet every 6 s, a probe every
    /// second, 90 s budget).
    fn wake(args: &[String]) -> u8 {
        let Some(reference) = positional(args, 0) else {
            eprintln!("usage: punktfunk wake <host-ref> [--wait]");
            return UNRESOLVED;
        };
        let (known, i) = match resolve(&reference) {
            Ok(v) => v,
            Err(code) => return code,
        };
        let host = &known.hosts[i];
        if host.mac.is_empty() {
            // A MAC is learned from the host's mDNS advert, never from a connect — say so, since
            // "connect to it once" sent at least one Deck owner looking in the wrong place (#322).
            eprintln!("no Wake-on-LAN address known for {} — run `punktfunk discover` while it's awake (the Deck panel does this every time it opens) so the client learns it from the host's advert", host.name);
            return UNRESOLVED;
        }
        if !has(args, "--wait") {
            wol::wake(&host.mac, host.addr.parse().ok());
            println!("sent a wake packet to {}", host.name);
            return OK;
        }
        let last =
            orchestrate::wake_by_probe(&host.addr, host.port, &host.fp_hex, &host.mac, |_| true);
        match last {
            Some(WakeTick {
                outcome: Some(WakeOutcome::Online),
                seconds,
                ..
            }) => {
                println!("{} is up after {seconds}s", host.name);
                OK
            }
            _ => {
                eprintln!(
                    "{} didn't come online within {WAKE_TIMEOUT_SECS}s",
                    host.name
                );
                CONNECT_FAILED
            }
        }
    }

    /// `library <host-ref> [--json]` — the host's games. TSV by default because that is what
    /// Decky's existing consumer parses; `--json` is the door for tools (the Playnite importer
    /// shells to exactly this).
    fn library_cmd(args: &[String]) -> u8 {
        let (known, i) = match resolve_or_default(args, "punktfunk library [<host-ref>] [--json]") {
            Ok(v) => v,
            Err(code) => return code,
        };
        let host = &known.hosts[i];
        let identity = match trust::load_or_create_identity() {
            Ok(id) => id,
            Err(e) => {
                eprintln!("client identity: {e:#}");
                return CONNECT_FAILED;
            }
        };
        let pin = trust::parse_hex32(&host.fp_hex);
        if pin.is_none() {
            eprintln!(
                "{} isn't paired yet — punktfunk pair {}",
                host.name, host.addr
            );
            return NEEDS_INTERACTION;
        }
        // The port this host actually serves its library on — learned from its advert and saved,
        // falling back to 47990. Reaching for the constant here is what broke a moved port.
        match library::fetch_games(&host.addr, host.effective_mgmt_port(), &identity, pin) {
            Ok(games) => {
                if has(args, "--json") {
                    let rows: Vec<serde_json::Value> = games
                        .iter()
                        .map(
                            |g| serde_json::json!({"id": g.id, "store": g.store, "title": g.title}),
                        )
                        .collect();
                    println!("{}", serde_json::json!({ "games": rows }));
                } else {
                    for g in &games {
                        println!("{}\t{}\t{}", g.id, g.store, g.title);
                    }
                    println!("{} game(s)", games.len());
                }
                OK
            }
            Err(e) => {
                eprintln!("library: {e}");
                CONNECT_FAILED
            }
        }
    }

    /// `end-game [<host-ref>] --game ID` — close a title this device launched on the host.
    fn end_game_cmd(args: &[String]) -> u8 {
        let usage = "punktfunk end-game [<host-ref>] --game ID";
        let Some(game) = value(args, "--game") else {
            eprintln!("usage: {usage}");
            return UNRESOLVED;
        };
        let (known, i) = match resolve_or_default(args, usage) {
            Ok(v) => v,
            Err(code) => return code,
        };
        let host = &known.hosts[i];
        let Some(pin) = trust::parse_hex32(&host.fp_hex) else {
            eprintln!(
                "{} isn't paired yet — punktfunk pair {}",
                host.name, host.addr
            );
            return NEEDS_INTERACTION;
        };
        let identity = match trust::load_or_create_identity() {
            Ok(id) => id,
            Err(e) => {
                eprintln!("client identity: {e:#}");
                return CONNECT_FAILED;
            }
        };
        let outcome = library::end_game(
            &host.addr,
            host.effective_mgmt_port(),
            &identity,
            Some(pin),
            &game,
        );
        println!("{}", outcome.notice(&game));
        match outcome {
            library::GameEnd::Ended => OK,
            library::GameEnd::NotRunning => UNRESOLVED,
            library::GameEnd::Expired => TRUST_REJECTED,
            library::GameEnd::Unsupported | library::GameEnd::Failed(_) => CONNECT_FAILED,
        }
    }

    /// `launch <host-ref> [--game ID] [--preset REF] [--exec]` — start a stream, wake included.
    /// `--exec` becomes the session process instead of supervising it: under a gamescope wrapper
    /// the launched process must BE the streaming one for focus and lifecycle to work.
    fn launch(args: &[String]) -> u8 {
        let exec = has(args, "--exec");
        let request_access = has(args, "--request-access");
        // Refused rather than silently downgraded: under `--exec` this process BECOMES the
        // session, so nothing survives to see `Ready` and record the approval. A launch that
        // quietly dropped the persistence would leave hosts reading "trusted" forever with
        // nobody able to say why.
        if request_access && exec {
            eprintln!(
                "--request-access can't be combined with --exec: under --exec there is no \
                 process left to record the host's approval"
            );
            return UNRESOLVED;
        }
        let usage = "punktfunk launch [<host-ref>] [--game ID] [--preset REF] [--exec]";
        let (known, i) = match resolve_or_default(args, usage) {
            Ok(v) => v,
            Err(code) => return code,
        };
        let mut plan = ConnectPlan::for_host(
            &known.hosts[i],
            value(args, "--game").as_deref(),
            value(args, "--preset")
                .or_else(|| value(args, "--profile"))
                .as_deref(),
        );
        if has(args, "--fullscreen") {
            plan.settings.fullscreen_on_stream = true;
        }
        if request_access {
            plan.connect_timeout_secs = Some(REQUEST_ACCESS_TIMEOUT_SECS);
        }
        run_plan(plan, exec, request_access)
    }

    /// `open <url>` — the `punktfunk://` grammar, headless. Same parser, same refusal rules and
    /// same connect path as a card click; what changes is only where the notices go.
    fn open(args: &[String]) -> u8 {
        let Some(url) = positional(args, 0) else {
            eprintln!("usage: punktfunk open <punktfunk://…>");
            return UNRESOLVED;
        };
        let link = match deeplink::parse(&url) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("{}", e.message());
                return UNRESOLVED;
            }
        };
        let known = KnownHosts::load();
        let outcome = orchestrate::plan_from_link(
            &link,
            &known,
            &PresetsFile::load(),
            &trust::Settings::load(),
        );
        match outcome {
            Ok(PlanOutcome::Connect(plan)) => run_plan(*plan, has(args, "--exec"), false),
            // The link named the host by something GUESSABLE — its label, its address — rather
            // than by its record id. A URL handed to us by someone else may not dial on a guess,
            // so a person says yes first. `--yes` is the scripted escape (and the only way in
            // without a terminal to ask on).
            Ok(PlanOutcome::ConfirmConnect(plan)) => {
                if !has(args, "--yes") {
                    if !is_tty() {
                        eprintln!(
                            "that link names {} by label or address, not by its id — re-run with \
                             --yes to open it",
                            plan.host.name
                        );
                        return NEEDS_INTERACTION;
                    }
                    eprint!("Connect to {} ({})? [y/N] ", plan.host.name, plan.host.addr);
                    let mut line = String::new();
                    if std::io::stdin().read_line(&mut line).is_err()
                        || !line.trim().eq_ignore_ascii_case("y")
                    {
                        eprintln!("cancelled");
                        return OK;
                    }
                }
                run_plan(*plan, has(args, "--exec"), false)
            }
            // A URL may never pair or trust on its own — that is a decision for a person, at a
            // surface that can show them the fingerprint.
            Ok(PlanOutcome::ConfirmUnknown(u)) => {
                eprintln!(
                    "{} isn't paired with this device — punktfunk pair {}:{}",
                    u.name.unwrap_or_else(|| u.addr.clone()),
                    u.addr,
                    u.port
                );
                NEEDS_INTERACTION
            }
            Ok(PlanOutcome::Unsupported(route)) => {
                eprintln!("punktfunk can't open \"{}\" links yet", route.as_str());
                UNRESOLVED
            }
            Err(e) => {
                eprintln!("{}", e.message());
                UNRESOLVED
            }
        }
    }

    /// Wake if needed, then run the session — supervising it, or becoming it under `--exec`.
    ///
    /// `persist_paired` records the host as *paired* when the child reports ready. Only
    /// `launch --request-access` passes true: there, the host parked the connect until an
    /// operator approved this device, so `Ready` IS the approval arriving — the same thing
    /// `SpawnOpts::persist_paired` means in the GTK shell. Every other launch records nothing,
    /// which is correct: a plain connect proves reachability, not a new trust decision.
    fn run_plan(plan: ConnectPlan, exec: bool, persist_paired: bool) -> u8 {
        if plan.host.fp_hex.is_none() {
            eprintln!(
                "{} has no pinned fingerprint — punktfunk pair {}",
                plan.host.name, plan.host.addr
            );
            return NEEDS_INTERACTION;
        }
        // Wake first when the host is asleep and we know how to reach it. This is the thing the
        // old exec-style CLI never did: it fired a packet at best and dialled into the void.
        let fp = plan.host.fp_hex.as_deref().unwrap_or_default();
        if plan.wake
            && !trust::probe_one(
                &plan.host.addr,
                plan.host.port,
                fp,
                Duration::from_millis(900),
            )
        {
            eprintln!("waking {}…", plan.host.name);
            let last = orchestrate::wake_by_probe(
                &plan.host.addr,
                plan.host.port,
                fp,
                &plan.host.mac,
                |_| true,
            );
            if last.and_then(|t| t.outcome) != Some(WakeOutcome::Online) {
                eprintln!("{} didn't come online", plan.host.name);
                return CONNECT_FAILED;
            }
        }
        if let Some(p) = &plan.preset {
            eprintln!("streaming with \"{}\"", p.name);
        }
        if exec {
            let e = orchestrate::exec_session(&plan);
            eprintln!("couldn't exec the session binary: {e}");
            return RENDERER_FAILED;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let spawned = orchestrate::spawn_session(&plan, None, move |ev| {
            let _ = tx.send(ev);
        });
        if let Err(e) = spawned {
            eprintln!("{e}");
            return RENDERER_FAILED;
        }
        let mut failure: Option<(String, bool)> = None;
        while let Ok(ev) = rx.recv() {
            match ev {
                SessionEvent::Ready => {
                    eprintln!("streaming");
                    // The pin we connected WITH, not one re-derived from the store: the record
                    // is what we are about to rewrite, and the session proved the host holds
                    // exactly this identity by completing a pinned handshake against it.
                    if persist_paired {
                        if let Some(fp_hex) = &plan.host.fp_hex {
                            if let Err(e) = trust::persist_host(
                                &plan.host.name,
                                &plan.host.addr,
                                plan.host.port,
                                fp_hex,
                                true,
                                &[],
                            ) {
                                eprintln!("couldn't save the host: {e:#}");
                            }
                            trust::forget_placeholder(&plan.host.addr, plan.host.port);
                        }
                    }
                }
                SessionEvent::Error {
                    msg,
                    trust_rejected,
                } => failure = Some((msg, trust_rejected)),
                SessionEvent::Ended(reason) => eprintln!("{reason}"),
                // The window size is persisted by the brain on the way past.
                SessionEvent::Window { .. } | SessionEvent::Stats(_) => {}
                // `ended` is printed as it arrives; its absence changes no exit code.
                SessionEvent::Exited(code) => {
                    return match ConnectOutcome::from_exit(code, failure.take(), None, false) {
                        ConnectOutcome::TrustRejected(msg) => {
                            eprintln!("{msg}");
                            TRUST_REJECTED
                        }
                        ConnectOutcome::ConnectFailed(msg) => {
                            eprintln!("{msg}");
                            CONNECT_FAILED
                        }
                        ConnectOutcome::RendererFailed { .. } => RENDERER_FAILED,
                        ConnectOutcome::Ended(_) | ConnectOutcome::Cancelled => OK,
                    };
                }
            }
        }
        OK
    }

    /// `reachable <host-ref>` — one bounded, mDNS-independent probe. Exit 0 = reachable.
    fn reachable(args: &[String]) -> u8 {
        let Some(reference) = positional(args, 0) else {
            eprintln!("usage: punktfunk reachable <host-ref>");
            return UNRESOLVED;
        };
        // An address that isn't saved is still a legitimate thing to probe — this verb answers
        // "can I reach this?", not "do I know this?". Resolved QUIETLY for the same reason:
        // `resolve`'s "pair it first" advice is for verbs that need a saved host, and printing
        // it here would scold the exact usage this verb documents.
        let known = KnownHosts::load();
        let link = DeepLink {
            host_ref: reference.clone(),
            ..Default::default()
        };
        let (addr, port) = match deeplink::resolve_host(&link, &known) {
            // Nothing is dialled and no title is launched, so a guessable reference needs no
            // confirmation — it only picks which address to send one probe packet to.
            HostResolution::Known(i) | HostResolution::Confirm(i) => {
                (known.hosts[i].addr.clone(), known.hosts[i].port)
            }
            _ => split_host_port(&reference),
        };
        if punktfunk_core::client::NativeClient::probe(&addr, port, PROBE_TIMEOUT) {
            println!("reachable {addr}:{port}");
            OK
        } else {
            eprintln!("unreachable {addr}:{port}");
            CONNECT_FAILED
        }
    }

    /// `speed-test <host-ref>` — measure the real data plane and print what it recommends.
    /// Deliberately does NOT apply the result: which layer a bitrate belongs in is a decision
    /// the GUI makes with the user (bound preset vs global, design §5.3), and a CLI silently
    /// rewriting a preset would be the surprise that rule exists to prevent.
    fn speed_test(args: &[String]) -> u8 {
        let Some(reference) = positional(args, 0) else {
            eprintln!("usage: punktfunk speed-test <host-ref>");
            return UNRESOLVED;
        };
        let (known, i) = match resolve(&reference) {
            Ok(v) => v,
            Err(code) => return code,
        };
        let host = &known.hosts[i];
        let Some(pin) = trust::parse_hex32(&host.fp_hex) else {
            eprintln!("{} isn't paired yet", host.name);
            return NEEDS_INTERACTION;
        };
        let identity = match trust::load_or_create_identity() {
            Ok(id) => id,
            Err(e) => {
                eprintln!("client identity: {e:#}");
                return CONNECT_FAILED;
            }
        };
        // Every Hello field at its default: this connect only runs the probe and never presents.
        let mode = punktfunk_core::config::Mode {
            width: 1280,
            height: 720,
            refresh_hz: 60,
        };
        let params = punktfunk_core::client::ConnectParams {
            name: Some(punktfunk_core::client::device_name()),
            pin: Some(pin),
            identity: Some(identity),
            ..punktfunk_core::client::ConnectParams::new(
                &host.addr,
                host.port,
                mode,
                Duration::from_secs(15),
            )
        };
        let client = match punktfunk_core::client::NativeClient::connect(params) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("connect: {e:?}");
                return CONNECT_FAILED;
            }
        };
        use punktfunk_core::client::health;
        let r = match health::speed_test(&client, |_| {}) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("speed test: {e:?}");
                return CONNECT_FAILED;
            }
        };
        let recommended = health::recommended_kbps(r.ceiling_kbps);
        if has(args, "--json") {
            println!(
                "{}",
                serde_json::json!({
                    "mbps": f64::from(r.ceiling_kbps) / 1000.0,
                    "wall": r.wall,
                    "clean": r.clean.map(|c| serde_json::json!({
                        "rate_mbps": f64::from(c.rate_kbps) / 1000.0,
                        "loss_pct": c.loss_pct,
                        "jitter_ms": f64::from(c.jitter_us) / 1000.0,
                        "reorders": c.reorders,
                    })),
                    "recommended_kbps": recommended,
                })
            );
        } else {
            let clean = match r.clean {
                Some(c) => format!(
                    " · at {:.0} Mbit/s: {:.1}% loss, {:.1} ms jitter",
                    f64::from(c.rate_kbps) / 1000.0,
                    c.loss_pct,
                    f64::from(c.jitter_us) / 1000.0
                ),
                None => String::new(),
            };
            println!(
                "{:.0} Mbit/s measured{clean} · recommended {:.0} Mbit/s",
                f64::from(r.ceiling_kbps) / 1000.0,
                f64::from(recommended) / 1000.0
            );
        }
        OK
    }

    /// `network-check <host-ref>` — the speed test and the shaped legs over a probe-only
    /// session, then every finding by id with its figures and the profile it offers.
    fn network_check(args: &[String]) -> u8 {
        use punktfunk_core::client::health::{self, FindingId, LegShape};
        use punktfunk_core::quic::{DeliveryAsk, EXT_DELIVERY_FACTS, EXT_DELIVERY_PROBE_ONLY};
        let Some(reference) = positional(args, 0) else {
            eprintln!("usage: punktfunk network-check <host-ref>");
            return UNRESOLVED;
        };
        let (known, i) = match resolve(&reference) {
            Ok(v) => v,
            Err(code) => return code,
        };
        let host = &known.hosts[i];
        let Some(pin) = trust::parse_hex32(&host.fp_hex) else {
            eprintln!("{} isn't paired yet", host.name);
            return NEEDS_INTERACTION;
        };
        let identity = match trust::load_or_create_identity() {
            Ok(id) => id,
            Err(e) => {
                eprintln!("client identity: {e:#}");
                return CONNECT_FAILED;
            }
        };
        let mode = punktfunk_core::config::Mode {
            width: 1280,
            height: 720,
            refresh_hz: 60,
        };
        let params = punktfunk_core::client::ConnectParams {
            name: Some(punktfunk_core::client::device_name()),
            pin: Some(pin),
            identity: Some(identity),
            delivery: Some(DeliveryAsk {
                profile: 0,
                flags: EXT_DELIVERY_FACTS | EXT_DELIVERY_PROBE_ONLY,
            }),
            ..punktfunk_core::client::ConnectParams::new(
                &host.addr,
                host.port,
                mode,
                Duration::from_secs(15),
            )
        };
        let client = match punktfunk_core::client::NativeClient::connect(params) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("connect: {e:?}");
                return CONNECT_FAILED;
            }
        };
        let r = match health::health_check(&client, |_| {}) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("network check: {e:?}");
                return CONNECT_FAILED;
            }
        };
        let label = |id: FindingId| match id {
            FindingId::SpeedMismatch => "the host's port is faster than this device's",
            FindingId::BurstIntolerant => "this device drops the head of a line-rate burst",
            FindingId::ReceiveBuffer => "loss in this device's receive buffer",
            FindingId::LinkFault => "loss at a rate no link refuses: cable, port or driver",
            FindingId::QueueBuildUp => "something on the path buffers",
            FindingId::HostSendBuffer => "the host's send buffer refused packets",
            FindingId::Wifi => "this device is on Wi-Fi",
        };
        let leg_name = |s: LegShape| match s {
            LegShape::FrameBursts => "bursts",
            LegShape::Capped => "capped",
        };
        if has(args, "--json") {
            println!(
                "{}",
                serde_json::json!({
                    "ceiling_mbps": f64::from(r.speed.ceiling_kbps) / 1000.0,
                    "wall": r.speed.wall,
                    "clean": r.speed.clean.map(|c| serde_json::json!({
                        "rate_mbps": f64::from(c.rate_kbps) / 1000.0,
                        "loss_pct": c.loss_pct,
                        "jitter_ms": f64::from(c.jitter_us) / 1000.0,
                        "reorders": c.reorders,
                    })),
                    "client": {
                        "iface_kind": r.client.link.kind,
                        "link_mbps": r.client.link.mbps,
                        "rcvbuf_kb": r.client.rcvbuf_kb,
                    },
                    "host": r.host.map(|h| serde_json::json!({
                        "iface_kind": h.iface_kind,
                        "link_mbps": h.link_mbps,
                        "sndbuf_kb": h.sndbuf_kb,
                        "forced_profile": h.forced_profile,
                    })),
                    "legs": r.legs.iter().map(|l| serde_json::json!({
                        "shape": leg_name(l.shape),
                        "loss_pct": l.outcome.loss_pct,
                        "jitter_ms": f64::from(l.outcome.gap_p99_us.saturating_sub(l.outcome.gap_p50_us)) / 1000.0,
                        "socket_drops": l.socket_drops,
                    })).collect::<Vec<_>>(),
                    "slow": r.slow.map(|s| serde_json::json!({
                        "loss_pct": s.loss_pct,
                        "jitter_ms": f64::from(s.gap_p99_us.saturating_sub(s.gap_p50_us)) / 1000.0,
                    })),
                    "findings": r.findings.iter().map(|f| serde_json::json!({
                        "id": f.id as u8,
                        "label": label(f.id),
                        "severity": f.severity as u8,
                        "numbers": f.numbers,
                        "profile": f.profile,
                    })).collect::<Vec<_>>(),
                })
            );
        } else {
            println!(
                "Link carries {}{:.0} Mbit/s.",
                if r.speed.wall { "" } else { "at least " },
                f64::from(r.speed.ceiling_kbps) / 1000.0
            );
            if let Some(c) = r.speed.clean {
                println!(
                    "At {:.0} Mbit/s: {:.1} % loss, {:.1} ms jitter.",
                    f64::from(c.rate_kbps) / 1000.0,
                    c.loss_pct,
                    f64::from(c.jitter_us) / 1000.0
                );
            }
            for l in &r.legs {
                println!(
                    "{}: {:.1} % loss{}",
                    leg_name(l.shape),
                    l.outcome.loss_pct,
                    l.socket_drops
                        .map(|d| format!(", {d} dropped at this socket"))
                        .unwrap_or_default()
                );
            }
            if r.findings.is_empty() {
                println!("Nothing to fix.");
            }
            for f in &r.findings {
                println!(
                    "- {}{}",
                    label(f.id),
                    match f.profile {
                        Some(1) => " — paced delivery (capped) would help",
                        Some(2) => " — paced delivery (smooth) would help",
                        _ => "",
                    }
                );
            }
        }
        OK
    }

    /// `presets list` — the presets this device has, and what each overrides. `legacy` is the
    /// `profiles` spelling, whose `--json` keeps the pre-rename `profiles` key.
    fn presets(args: &[String], legacy: bool) -> u8 {
        match positional(args, 0).as_deref() {
            Some("list") | None => {
                let catalog = PresetsFile::load();
                if has(args, "--json") && legacy {
                    let json = serde_json::json!({
                        "version": catalog.version,
                        "profiles": catalog.presets,
                    });
                    println!("{json}");
                } else if has(args, "--json") {
                    println!(
                        "{}",
                        serde_json::to_string(&catalog).unwrap_or_else(|_| "{}".into())
                    );
                } else {
                    for p in &catalog.presets {
                        let n = serde_json::to_value(&p.overrides)
                            .ok()
                            .and_then(|v| v.as_object().map(|o| o.len()))
                            .unwrap_or(0);
                        println!("{}\t{}\t{n} override(s)", p.id, p.name);
                    }
                }
                OK
            }
            Some(other) => {
                eprintln!("unknown presets command \"{other}\" — list");
                UNRESOLVED
            }
        }
    }

    /// `reset` — forget this device's saved hosts and stream settings. The identity (and so the
    /// hosts' record of this device) is deliberately NOT touched: re-pairing is the user's call.
    fn reset() -> u8 {
        if !is_tty() {
            eprintln!("refusing to reset without a terminal to confirm on");
            return NEEDS_INTERACTION;
        }
        eprint!("Forget every saved host and reset settings? [y/N] ");
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line).is_err() || !line.trim().eq_ignore_ascii_case("y")
        {
            eprintln!("cancelled");
            return OK;
        }
        let mut known = KnownHosts::load();
        known.hosts.clear();
        let _ = known.save();
        trust::Settings::default().save();
        println!("client state reset");
        OK
    }

    /// A user-typed `<host[:port]>`. Shared parser: a plain `rsplit_once(':')` reads the
    /// bare IPv6 `::1` as host `:` port `1`, and this value goes on to be dialled and stored.
    fn split_host_port(target: &str) -> (String, u16) {
        pf_client_core::deeplink::parse_addr_port(target)
            .unwrap_or_else(|| (target.to_string(), pf_client_core::deeplink::DEFAULT_PORT))
    }

    /// Is stdin a terminal? Decides whether a verb may ask a question or must refuse with
    /// [`NEEDS_INTERACTION`] — a CLI that blocks a CI job on a prompt is a hang, not a UX.
    fn is_tty() -> bool {
        std::io::IsTerminal::is_terminal(&std::io::stdin())
    }

    /// One line of PIN from stdin — prompted when we're asking a person, silent for `--pin -`
    /// (a pipe from another program). `None` on a read error or an empty line (EOF), which the
    /// caller turns into [`NEEDS_INTERACTION`] rather than sending an empty PIN to the host.
    fn read_pin(prompt_for: Option<&str>) -> Option<String> {
        if let Some(addr) = prompt_for {
            eprint!("PIN shown on {addr}: ");
        }
        let mut line = String::new();
        std::io::stdin().read_line(&mut line).ok()?;
        let pin = line.trim();
        (!pin.is_empty()).then(|| pin.to_string())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// The other OS of a dual-boot box answers at the saved one's lease with its own pin.
        /// It is not that record, so `discover` must not call it saved or paired.
        #[test]
        fn a_second_os_at_a_saved_address_is_not_saved() {
            let saved = KnownHost {
                addr: "192.168.1.9".into(),
                port: 9777,
                fp_hex: "aa".into(),
                paired: true,
                ..Default::default()
            };
            let known = KnownHosts { hosts: vec![saved] };
            let mut advert = pf_client_core::discovery::DiscoveredHost {
                key: "id-2".into(),
                fullname: "desk._punktfunk._udp.local.".into(),
                name: "desk".into(),
                addr: "192.168.1.9".into(),
                port: 9777,
                fp_hex: "bb".into(),
                pair: "required".into(),
                mgmt_port: None,
                mac: vec![],
                os: String::new(),
            };
            assert!(match_saved(&known, &advert).is_none());
            advert.fp_hex = "AA".into();
            advert.addr = "192.168.1.20".into();
            assert!(
                match_saved(&known, &advert).is_some(),
                "a moved lease keeps its record"
            );
            advert.fp_hex = String::new();
            advert.addr = "192.168.1.9".into();
            assert!(
                match_saved(&known, &advert).is_some(),
                "no pin: the address decides"
            );
        }

        fn argv(v: &[&str]) -> Vec<String> {
            v.iter().map(|s| s.to_string()).collect()
        }

        /// Flags and their values never masquerade as the verb's subject — the bug that makes
        /// `launch --preset Work desk` reach for a host called "Work".
        #[test]
        fn positional_skips_flags_and_their_values() {
            assert_eq!(
                positional(&argv(&["desk", "--game", "steam:570"]), 0),
                Some("desk".into())
            );
            for flag in ["--preset", "--profile"] {
                assert_eq!(
                    positional(&argv(&[flag, "Work", "desk"]), 0),
                    Some("desk".into())
                );
            }
            assert_eq!(
                positional(&argv(&["--exec", "desk"]), 0),
                Some("desk".into()),
                "a valueless flag must not swallow the subject"
            );
            assert_eq!(
                positional(&argv(&["add", "10.0.0.1"]), 1),
                Some("10.0.0.1".into())
            );
            assert_eq!(positional(&argv(&["--json"]), 0), None);
            // `--pin -` (the PIN comes down stdin, never argv): the lone dash is that flag's
            // VALUE, not the host to pair with.
            assert_eq!(
                positional(&argv(&["--pin", "-", "desk"]), 0),
                Some("desk".into())
            );
            assert_eq!(
                value(&argv(&["desk", "--pin", "-"]), "--pin"),
                Some("-".into())
            );
        }

        #[test]
        fn host_port_splitting() {
            assert_eq!(split_host_port("desk"), ("desk".into(), 9777));
            assert_eq!(split_host_port("desk:1234"), ("desk".into(), 1234));
            // Not a port: keep the whole thing as the address rather than inventing one.
            assert_eq!(split_host_port("desk:nope"), ("desk:nope".into(), 9777));
        }

        /// Every advertised verb documents itself — a USAGE line without a help entry is a
        /// promise `help <verb>` breaks. The overview and each entry must also name the verb.
        #[test]
        fn every_usage_verb_has_help() {
            for verb in [
                "discover",
                "pair",
                "hosts",
                "default-host",
                "wake",
                "library",
                "launch",
                "open",
                "reachable",
                "speed-test",
                "network-check",
                "presets",
                "reset",
            ] {
                let h = verb_help(verb).unwrap_or_else(|| panic!("no help for {verb}"));
                assert!(
                    h.starts_with(&format!("punktfunk {verb}")),
                    "help for {verb} must lead with its own invocation"
                );
                assert!(USAGE.contains(verb), "USAGE must advertise {verb}");
            }
            assert!(verb_help("bogus").is_none());
        }

        fn saved(name: &str, addr: &str, fp: &str) -> KnownHost {
            KnownHost {
                name: name.into(),
                addr: addr.into(),
                port: 9777,
                fp_hex: fp.into(),
                ..Default::default()
            }
        }

        /// Step 1 of request access: a host saved by address gains the fingerprint its advert
        /// carried. Before this, `hosts add --fp` on an existing record exited 0 having done
        /// NOTHING — the launch that followed then refused for want of a pin, and the panel had
        /// no way to tell why.
        #[test]
        fn adding_a_fingerprint_to_a_placeholder_fills_it_in() {
            let mut known = KnownHosts {
                hosts: vec![saved("192.168.1.9", "192.168.1.9", "")],
            };
            assert_eq!(
                merge_saved_host(&mut known, 0, "abc123", Some("living-room")),
                AddOutcome::Pinned
            );
            assert_eq!(known.hosts[0].fp_hex, "abc123");
            assert_eq!(
                known.hosts[0].name, "living-room",
                "a record still named after its address takes the offered label"
            );
        }

        /// A label the user chose is theirs — an advert's name must not overwrite it.
        #[test]
        fn filling_in_a_fingerprint_keeps_a_user_chosen_name() {
            let mut known = KnownHosts {
                hosts: vec![saved("Basement rig", "192.168.1.9", "")],
            };
            merge_saved_host(&mut known, 0, "abc123", Some("living-room"));
            assert_eq!(known.hosts[0].name, "Basement rig");
        }

        /// Idempotent: the panel may retry step 1, and re-offering the fingerprint a record
        /// already carries is a state that is already correct, not an error to render.
        #[test]
        fn re_adding_the_same_fingerprint_changes_nothing() {
            let mut known = KnownHosts {
                hosts: vec![saved("desk", "192.168.1.9", "ABC123")],
            };
            assert_eq!(
                merge_saved_host(&mut known, 0, "abc123", None),
                AddOutcome::Unchanged,
                "fingerprints compare case-insensitively"
            );
            // And a bare `hosts add` with no --fp at all leaves the pin alone.
            assert_eq!(
                merge_saved_host(&mut known, 0, "", None),
                AddOutcome::Unchanged
            );
            assert_eq!(known.hosts[0].fp_hex, "ABC123");
        }

        /// Both OS installs of a dual-boot box answer at one address with a certificate each.
        /// The second `hosts add --fp` is not about the first's record, so it is filed beside
        /// it — the pin the user already has must survive, and so must the new one.
        #[test]
        fn a_second_identity_at_one_address_is_not_this_adds_target() {
            let known = KnownHosts {
                hosts: vec![saved("desk", "192.168.1.9", "abc123")],
            };
            assert_eq!(add_target(&known, "192.168.1.9", 9777, "deadbeef"), None);
            // The same host again, and a placeholder waiting for a pin, both ARE the target.
            assert_eq!(add_target(&known, "192.168.1.9", 9777, "ABC123"), Some(0));
            assert_eq!(add_target(&known, "192.168.1.9", 9777, ""), Some(0));
            let placeholder = KnownHosts {
                hosts: vec![saved("192.168.1.9", "192.168.1.9", "")],
            };
            assert_eq!(
                add_target(&placeholder, "192.168.1.9", 9777, "abc"),
                Some(0)
            );
        }

        /// A changed identity is a decision for a person. Never a silent overwrite — this is the
        /// same rule `upsert_trusted` enforces, and a back door here would defeat it everywhere.
        #[test]
        fn a_different_fingerprint_is_refused_not_overwritten() {
            let mut known = KnownHosts {
                hosts: vec![saved("desk", "192.168.1.9", "abc123")],
            };
            assert_eq!(
                merge_saved_host(&mut known, 0, "deadbeef", None),
                AddOutcome::Conflict
            );
            assert_eq!(
                known.hosts[0].fp_hex, "abc123",
                "the pin must survive intact"
            );
        }

        /// A host that changed DHCP lease is re-pointed, not filed a second time. Without this
        /// the record a stable id resolves to keeps an address the host has left, so a launch
        /// dials into the void while the panel shows the live one.
        #[test]
        fn a_known_fingerprint_at_a_new_address_moves_the_record() {
            let mut known = KnownHosts {
                hosts: vec![saved("desk", "192.168.1.9", "abc123")],
            };
            // Simulates `hosts add 192.168.1.50 --fp abc123` finding no record at that address.
            let by_addr = known
                .hosts
                .iter()
                .position(|h| h.addr == "192.168.1.50" && h.port == 9777);
            assert!(
                by_addr.is_none(),
                "the new address is not yet on any record"
            );
            let by_fp = known
                .hosts
                .iter()
                .position(|h| h.fp_hex.eq_ignore_ascii_case("abc123"));
            assert_eq!(by_fp, Some(0), "the fingerprint still identifies the host");
            known.hosts[0].addr = "192.168.1.50".into();
            assert_eq!(known.hosts.len(), 1, "one host, one record");
        }

        #[test]
        fn value_reads_the_argument_after_its_flag() {
            let a = argv(&["--game", "steam:570", "--exec"]);
            assert_eq!(value(&a, "--game"), Some("steam:570".into()));
            assert_eq!(value(&a, "--preset"), None);
            // A flag followed by another flag has no value.
            assert_eq!(value(&argv(&["--preset", "--exec"]), "--preset"), None);
            assert!(has(&a, "--exec"));
        }
    }
}

#[cfg(any(target_os = "linux", windows))]
fn main() -> std::process::ExitCode {
    punktfunk_core::tls::install_default_provider();
    // Logs to stderr; stdout is the machine interface (TSV/JSON), exactly like the session
    // binary's contract.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    std::process::ExitCode::from(cli::run(args))
}

/// Keeps `cargo build --workspace` green on macOS, where the client is clients/apple.
#[cfg(not(any(target_os = "linux", windows)))]
fn main() {
    eprintln!("punktfunk runs on Linux and Windows — the macOS client lives in clients/apple");
    std::process::exit(2);
}
