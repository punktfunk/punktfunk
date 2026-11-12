//! Runs the REAL `punktfunk` binary and pins its self-documentation contract: help goes
//! to stdout and exits 0, an unknown verb refuses on stderr with the not-found code.
//!
//! This exists so CI *executes* the shipped binary at least once per platform. A binary
//! that compiles but is the wrong program passes every build/clippy/fmt gate — that is
//! exactly how 0.22.0 shipped a stub as `punktfunk-session` — and only a gate that runs
//! the thing catches the class. The help paths are the right probe: they touch no config
//! stores and no network, so they are safe on any runner.
#![cfg(any(target_os = "linux", windows))]

use std::process::Command;

fn punktfunk(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_punktfunk"))
        .args(args)
        .output()
        .expect("run punktfunk")
}

#[test]
fn bare_and_help_print_the_overview_on_stdout() {
    for args in [&[][..], &["help"][..], &["--help"][..], &["-h"][..]] {
        let out = punktfunk(args);
        assert!(out.status.success(), "{args:?} must exit 0");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains("punktfunk pair"),
            "{args:?} overview lists the verbs"
        );
        assert!(
            stdout.contains("help <command>"),
            "{args:?} overview points at per-verb help"
        );
    }
}

#[test]
fn per_verb_help_answers_both_spellings() {
    for args in [
        &["help", "launch"][..],
        &["launch", "--help"][..],
        &["launch", "-h"][..],
    ] {
        let out = punktfunk(args);
        assert!(out.status.success(), "{args:?} must exit 0");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains("--exec"),
            "{args:?} documents launch's flags"
        );
    }
}

/// `default-host` is the only door to the start-screen pointer on a headless box, and the
/// verbs that read it must refuse rather than guess. Runs against a scratch directory.
/// `PUNKTFUNK_CONFIG_DIR` is set there too, so a developer export cannot point the
/// verb at the real identity.
#[test]
fn default_host_is_set_read_and_cleared() {
    let home = std::env::temp_dir().join(format!("pf-cli-default-host-{}", std::process::id()));
    let store = home.join(if cfg!(windows) {
        "punktfunk"
    } else {
        ".config/punktfunk"
    });
    std::fs::create_dir_all(&store).expect("scratch config dir");
    std::fs::write(
        store.join("client-known-hosts.json"),
        r#"{"hosts":[{"name":"Desk","addr":"10.0.0.5","port":9777,
           "fp_hex":"aa","paired":true,"id":"rec-1"}]}"#,
    )
    .expect("seed the store");

    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_punktfunk"))
            .args(args)
            .env(if cfg!(windows) { "APPDATA" } else { "HOME" }, &home)
            .env("PUNKTFUNK_CONFIG_DIR", &store)
            .output()
            .expect("run punktfunk")
    };

    // One paired host derives, with nothing written.
    let out = run(&["default-host"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Desk") && stdout.contains("derived"),
        "{stdout}"
    );

    let out = run(&["default-host", "Desk"]);
    assert!(
        out.status.success(),
        "naming a paired host must be accepted"
    );
    assert!(String::from_utf8_lossy(&run(&["default-host"]).stdout).contains("explicit"));

    let out = run(&["default-host", "--clear"]);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&run(&["default-host"]).stdout).contains("derived"));

    // No records at all: nothing to derive, and the verbs that read it say so.
    std::fs::write(store.join("client-known-hosts.json"), r#"{"hosts":[]}"#).expect("empty store");
    assert!(String::from_utf8_lossy(&run(&["default-host"]).stdout).contains("none"));
    let out = run(&["library"]);
    assert_eq!(out.status.code(), Some(5), "no default host is not-found");
    assert!(String::from_utf8_lossy(&out.stderr).contains("no default host"));

    std::fs::remove_dir_all(&home).ok();
}

/// `hosts forget` takes what is keyed on the record with it: a default pointing at it and its
/// cached game list, which a re-pair of another box would otherwise inherit.
#[test]
fn forgetting_a_host_clears_its_default_and_cached_catalog() {
    let home = std::env::temp_dir().join(format!("pf-cli-forget-{}", std::process::id()));
    let store = home.join("config");
    let fp = "ab".repeat(32);
    let catalog = if cfg!(windows) {
        home.join("punktfunk/cache/library")
    } else {
        home.join("punktfunk/library")
    };
    std::fs::create_dir_all(&store).expect("scratch config dir");
    std::fs::create_dir_all(&catalog).expect("scratch cache dir");
    std::fs::write(
        store.join("client-known-hosts.json"),
        format!(r#"{{"hosts":[{{"name":"Desk","addr":"10.0.0.5","port":9777,"fp_hex":"{fp}","paired":true,"id":"rec-1"}}]}}"#),
    )
    .expect("seed the store");
    let settings_file = store.join(if cfg!(windows) {
        "client-windows-settings.json"
    } else {
        "client-gtk-settings.json"
    });
    std::fs::write(&settings_file, r#"{"default_host":"rec-1"}"#).expect("seed the default");
    let cached = catalog.join(format!("{fp}.json"));
    std::fs::write(&cached, r#"{"games":[],"fetched_at":0}"#).expect("seed the catalog");

    let out = Command::new(env!("CARGO_BIN_EXE_punktfunk"))
        .args(["hosts", "forget", "Desk"])
        .env("PUNKTFUNK_CONFIG_DIR", &store)
        .env("XDG_CACHE_HOME", &home)
        .env("LOCALAPPDATA", &home)
        .output()
        .expect("run punktfunk");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let settings = std::fs::read_to_string(&settings_file).expect("settings written back");
    assert!(!settings.contains("rec-1"), "{settings}");
    assert!(!cached.exists(), "the cached catalog outlived the host");

    std::fs::remove_dir_all(&home).ok();
}

/// Eight `hosts add` at once all land. Each is its own process, so only the store's file
/// lock keeps one's read, edit and save from running inside another's.
#[test]
fn hosts_added_by_eight_processes_at_once_all_land() {
    let store = std::env::temp_dir().join(format!("pf-cli-lock-{}", std::process::id()));
    std::fs::create_dir_all(&store).expect("scratch config dir");
    let adds: Vec<_> = (1..=8)
        .map(|n| {
            Command::new(env!("CARGO_BIN_EXE_punktfunk"))
                .args(["hosts", "add", &format!("10.0.0.{n}")])
                .env("PUNKTFUNK_CONFIG_DIR", &store)
                .spawn()
                .expect("run punktfunk")
        })
        .collect();
    for mut add in adds {
        assert!(add.wait().expect("wait for punktfunk").success());
    }
    let saved = std::fs::read_to_string(store.join("client-known-hosts.json")).expect("the store");
    assert_eq!(saved.matches("\"addr\"").count(), 8, "{saved}");

    std::fs::remove_dir_all(&store).ok();
}

#[test]
fn version_prints_the_crate_version() {
    let out = punktfunk(&["--version"]);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains(env!("CARGO_PKG_VERSION")));
}

#[test]
fn unknown_verbs_refuse_with_the_not_found_code() {
    let out = punktfunk(&["frobnicate"]);
    assert_eq!(out.status.code(), Some(5), "unknown verb exits 5");
    assert!(String::from_utf8_lossy(&out.stderr).contains("unknown command"));

    let out = punktfunk(&["help", "frobnicate"]);
    assert_eq!(out.status.code(), Some(5), "unknown help topic exits 5");
}

/// `--json` adds the stable shape a panel reads in place of the usage prose, which stays on
/// stderr, and the exit code stays 5.
#[test]
fn an_unknown_verb_under_json_names_itself_on_stdout() {
    let out = punktfunk(&["frobnicate", "--json"]);
    assert_eq!(out.status.code(), Some(5));
    assert!(String::from_utf8_lossy(&out.stderr).contains("unknown command"));
    let got: serde_json::Value = serde_json::from_slice(&out.stdout).expect("one JSON object");
    assert_eq!(
        got,
        serde_json::json!({"ok": false, "error": "unknown-command"})
    );
    assert!(punktfunk(&["frobnicate"]).stdout.is_empty());
}

/// `discover` and `launch --request-access` document themselves. Help only — the verbs
/// themselves browse the LAN and dial a host, which no runner may be asked to do.
///
/// The Decky panel detects a too-old client by exactly the signature the test above pins
/// (exit 5 + `unknown command`), so this is the other half of that contract: on a client new
/// enough, `discover` is a verb with help rather than an unknown word.
#[test]
fn the_request_access_surfaces_document_themselves() {
    let out = punktfunk(&["help", "discover"]);
    assert!(out.status.success(), "discover has its own help topic");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("--timeout"), "discover documents --timeout");
    assert!(stdout.contains("--json"), "discover documents --json");

    let out = punktfunk(&["launch", "--help"]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("--request-access"));
}

/// A scratch config dir holding `known_hosts` (the saved-hosts JSON, or `""` for none).
fn scratch_store(tag: &str, known_hosts: &str) -> std::path::PathBuf {
    let store = std::env::temp_dir().join(format!("pf-cli-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&store).expect("scratch config dir");
    if !known_hosts.is_empty() {
        std::fs::write(store.join("client-known-hosts.json"), known_hosts).expect("seed the store");
    }
    store
}

/// `punktfunk` against `store`, with every cache dir inside it too.
fn punktfunk_in(store: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_punktfunk"))
        .args(args)
        .env("PUNKTFUNK_CONFIG_DIR", store)
        .env("XDG_CACHE_HOME", store)
        .env("LOCALAPPDATA", store)
        .output()
        .expect("run punktfunk")
}

fn stdout_of(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr_of(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

const TWO_HOSTS: &str = r#"{"hosts":[
  {"name":"Desk","addr":"10.0.0.5","port":9777,"fp_hex":"aa","paired":true,"id":"rec-1"},
  {"name":"Couch","addr":"10.0.0.6","port":1234,"fp_hex":"","paired":false,"id":"rec-2"}]}"#;

/// `hosts list` without `--probe`: one `name addr:port paired|trusted -` line per record, in
/// store order, and a bare `hosts` is the same verb.
#[test]
fn hosts_list_prints_one_line_per_saved_host() {
    let store = scratch_store("list", TWO_HOSTS);
    let want = "Desk\t10.0.0.5:9777\tpaired\t-\nCouch\t10.0.0.6:1234\ttrusted\t-\n";
    for args in [&["hosts", "list"][..], &["hosts"][..]] {
        let out = punktfunk_in(&store, args);
        assert!(out.status.success(), "{args:?}: {}", stderr_of(&out));
        assert_eq!(stdout_of(&out), want, "{args:?}");
    }
    std::fs::remove_dir_all(&store).ok();
}

/// `hosts list --json`: the keys a panel reads, the preset and its pins resolved against the
/// catalog (a dangling pin drops out), the pre-rename `profile` keys mirroring them, and
/// `online` null when nothing was probed.
#[test]
fn hosts_list_json_resolves_presets_and_keeps_the_old_keys() {
    let store = scratch_store(
        "list-json",
        r#"{"hosts":[{"name":"Desk","addr":"10.0.0.5","port":9777,"fp_hex":"aa","paired":true,
           "id":"rec-1","mac":["aa:bb:cc:dd:ee:ff"],"os":"linux","last_used":1700000000,
           "clipboard_sync":true,"preset_id":"p1","pinned_presets":["p1","gone"]}]}"#,
    );
    std::fs::write(
        store.join("client-presets.json"),
        r#"{"version":1,"presets":[{"id":"p1","name":"Work"}]}"#,
    )
    .expect("seed the catalog");
    let out = punktfunk_in(&store, &["hosts", "list", "--json"]);
    assert!(out.status.success(), "{}", stderr_of(&out));
    let got: serde_json::Value = serde_json::from_str(&stdout_of(&out)).expect("one JSON object");
    let preset = serde_json::json!({"id": "p1", "name": "Work"});
    assert_eq!(
        got,
        serde_json::json!({"hosts": [{
            "id": "rec-1",
            "name": "Desk",
            "addr": "10.0.0.5",
            "port": 9777,
            "fp_hex": "aa",
            "paired": true,
            "mac": ["aa:bb:cc:dd:ee:ff"],
            "os": "linux",
            "last_used": 1_700_000_000,
            "clipboard_sync": true,
            "preset": preset,
            "pinned_presets": [preset],
            "profile": preset,
            "pinned_profiles": [preset],
            "online": null,
        }]})
    );
    std::fs::remove_dir_all(&store).ok();
}

/// What `hosts add` says for each thing it can do to the store, in the order one host's life
/// goes: filed, repeated, named, pinned, moved to a new lease, repeated at the new lease.
#[test]
fn hosts_add_reports_what_it_did() {
    let store = scratch_store("add", "");
    let fp = "ab".repeat(32);
    let say = |args: &[&str]| punktfunk_in(&store, args);
    for (args, stdout, stderr) in [
        (
            vec!["hosts", "add", "10.0.0.5"],
            "added 10.0.0.5:9777\n",
            "",
        ),
        (
            vec!["hosts", "add", "10.0.0.5"],
            "",
            "10.0.0.5:9777 is already saved\n",
        ),
        (
            vec!["hosts", "add", "10.0.0.5", "--name", "Desk"],
            "updated 10.0.0.5:9777\n",
            "",
        ),
        (
            vec!["hosts", "add", "10.0.0.5", "--fp", fp.as_str()],
            "updated 10.0.0.5:9777\n",
            "",
        ),
        (
            vec!["hosts", "add", "10.0.0.9", "--fp", fp.as_str()],
            "moved 10.0.0.5:9777 to 10.0.0.9:9777\n",
            "",
        ),
        (
            vec!["hosts", "add", "10.0.0.9", "--fp", fp.as_str()],
            "",
            "10.0.0.9:9777 is already saved\n",
        ),
    ] {
        let out = say(&args);
        assert_eq!(out.status.code(), Some(0), "{args:?}: {}", stderr_of(&out));
        assert_eq!(stdout_of(&out), stdout, "{args:?}");
        assert_eq!(stderr_of(&out), stderr, "{args:?}");
    }
    let out = say(&["hosts", "list"]);
    assert_eq!(stdout_of(&out), "Desk\t10.0.0.9:9777\ttrusted\t-\n");

    let out = say(&["hosts", "add", "10.0.0.9", "--fp", "zz"]);
    assert_eq!(out.status.code(), Some(5));
    assert!(stderr_of(&out).contains("--fp takes the host's 64-hex-digit fingerprint"));
    let out = say(&["hosts", "add"]);
    assert_eq!(out.status.code(), Some(5));
    assert!(stderr_of(&out).contains("usage: punktfunk hosts add"));
    std::fs::remove_dir_all(&store).ok();
}

/// A request with no fingerprint to hold the host to refuses before it dials: no `--fp`, and
/// nothing saved at the address.
#[test]
fn a_request_without_a_fingerprint_refuses() {
    let store = scratch_store("no-fp-request", "");
    let out = punktfunk_in(&store, &["pair", "127.0.0.1:1", "--request-access"]);
    assert_eq!(out.status.code(), Some(5), "{}", stderr_of(&out));
    assert!(stderr_of(&out).starts_with("no fingerprint for 127.0.0.1"));
    assert!(!store.join("client-known-hosts.json").exists());
    std::fs::remove_dir_all(&store).ok();
}

/// A `--fp` with nothing after it is a usage error, not a request without a pin.
#[test]
fn a_bare_fp_is_a_usage_error() {
    let store = scratch_store("bare-fp", "");
    for args in [
        &["pair", "127.0.0.1:1", "--request-access", "--fp"][..],
        &["pair", "127.0.0.1:1", "--fp", "--request-access"][..],
        &["hosts", "add", "10.0.0.5", "--fp"][..],
        &["hosts", "add", "10.0.0.5", "--fp", "--name", "Desk"][..],
    ] {
        let out = punktfunk_in(&store, args);
        assert_eq!(out.status.code(), Some(5), "{args:?}: {}", stderr_of(&out));
        assert_eq!(
            stderr_of(&out),
            "--fp takes the host's 64-hex-digit fingerprint\n",
            "{args:?}"
        );
    }
    assert!(!store.join("client-known-hosts.json").exists());
    std::fs::remove_dir_all(&store).ok();
}

/// The refusals of `hosts`: an unknown sub-command, a forget with nothing to forget.
#[test]
fn hosts_refuses_what_it_cannot_resolve() {
    let store = scratch_store("refuse", TWO_HOSTS);
    for (args, said) in [
        (&["hosts", "frob"][..], "unknown hosts command \"frob\""),
        (&["hosts", "forget"][..], "usage: punktfunk hosts forget"),
    ] {
        let out = punktfunk_in(&store, args);
        assert_eq!(out.status.code(), Some(5), "{args:?}");
        assert!(
            stderr_of(&out).contains(said),
            "{args:?}: {}",
            stderr_of(&out)
        );
    }
    // A name nothing matches reads as an address nobody saved: a person's call (6).
    let out = punktfunk_in(&store, &["hosts", "forget", "Nobody"]);
    assert_eq!(out.status.code(), Some(6));
    assert!(stderr_of(&out).contains("Nobody:9777 isn't a saved host"));
    let out = punktfunk_in(&store, &["hosts", "forget", "Desk"]);
    assert_eq!(stdout_of(&out), "forgot Desk\n");
    let left = stdout_of(&punktfunk_in(&store, &["hosts", "list"]));
    assert_eq!(left.lines().count(), 1, "{left}");
    std::fs::remove_dir_all(&store).ok();
}

/// `speed-test` and `network-check` stop before they dial: no reference, a reference nothing
/// matches, a saved host with no pin. Neither needs a network.
#[test]
fn the_probe_verbs_refuse_before_they_dial() {
    let store = scratch_store("probe", TWO_HOSTS);
    for verb in ["speed-test", "network-check"] {
        let out = punktfunk_in(&store, &[verb]);
        assert_eq!(out.status.code(), Some(5), "{verb}");
        assert!(
            stderr_of(&out).contains(&format!("usage: punktfunk {verb} <host-ref>")),
            "{verb}: {}",
            stderr_of(&out)
        );
        let out = punktfunk_in(&store, &[verb, "Nobody"]);
        assert_eq!(out.status.code(), Some(6), "{verb} Nobody");
        let out = punktfunk_in(&store, &[verb, "Couch"]);
        assert_eq!(out.status.code(), Some(6), "{verb} Couch");
        assert_eq!(stderr_of(&out), "Couch isn't paired yet\n", "{verb} Couch");
    }
    std::fs::remove_dir_all(&store).ok();
}

/// `hosts add` with no config directory to save into (no `HOME`, no `APPDATA`) saves nothing,
/// and says so with the connect-failed code rather than "already saved".
#[test]
fn hosts_add_without_a_config_dir_fails_to_save() {
    let out = Command::new(env!("CARGO_BIN_EXE_punktfunk"))
        .args(["hosts", "add", "10.0.0.5"])
        .env_remove("PUNKTFUNK_CONFIG_DIR")
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("HOME")
        .env_remove("APPDATA")
        .output()
        .expect("run punktfunk");
    assert_eq!(out.status.code(), Some(2), "{}", stderr_of(&out));
    assert!(
        stderr_of(&out).starts_with("saving: "),
        "{}",
        stderr_of(&out)
    );
}
