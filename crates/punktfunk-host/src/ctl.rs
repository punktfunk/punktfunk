//! `punktfunk-host ctl` — operator verbs over the local management API.
//!
//! Approve, PIN, rename, unpair, access, stop, watch: a loopback client of mgmt, not a
//! second binary. `main.rs` already dispatches verbs, and the client deserialises the
//! types the server serialises. Lift `ctl/` behind a thin bin if `ctl status --json` p50
//! exceeds ~150 ms; the module boundary makes that mechanical.
//!
//! Token and pin come from the host config directory (see [`client`]). `approve`/`deny`
//! take an id, never "the newest".
//!
//! Exit codes: 0 ok, 1 host refused, 2 usage, 3 no host, 4 pin mismatch. 4 is distinct
//! so a "host down" retry does not walk into a squatter.

pub mod client;
mod watch;

use client::{Client, Failure, Result, SCHEMA_VERSION};
use punktfunk_core::quic::{
    GRANT_PRESET_CONTROLLER_ONLY, GRANT_PRESET_FULL, GRANT_PRESET_VIEW_ONLY,
};
use serde_json::{json, Value};

pub fn main(args: &[String]) -> anyhow::Result<()> {
    // `--json` is a mode, not an argument of any verb.
    let json = args.iter().any(|a| a == "--json");
    let rest: Vec<&str> = args
        .iter()
        .map(String::as_str)
        .filter(|a| *a != "--json")
        .collect();
    match run(&rest, json) {
        Ok(()) => Ok(()),
        Err(f) => {
            if json {
                // JSON errors share the success envelope on stdout so a QML Process parses one shape.
                println!(
                    "{}",
                    json!({"v": SCHEMA_VERSION, "error": {"code": f.code, "message": f.message}})
                );
            } else {
                eprintln!("punktfunk-host ctl: {}", f.message);
            }
            std::process::exit(f.code)
        }
    }
}

fn run(args: &[&str], json: bool) -> Result<()> {
    let Some(verb) = args.first().copied() else {
        print_usage();
        return Err(Failure::usage("no verb given"));
    };
    let rest = &args[1..];
    match verb {
        "-h" | "--help" | "help" => {
            print_usage();
            Ok(())
        }
        "status" => {
            let v = Client::connect(None)?.get("/api/v1/status")?;
            out(json, &v, render_status);
            Ok(())
        }
        "sessions" => {
            let v = Client::connect(None)?.get("/api/v1/status")?;
            let slice = json!({
                "active_sessions": v.get("active_sessions").cloned().unwrap_or(Value::Null),
                "video_streaming": v.get("video_streaming").cloned().unwrap_or(Value::Null),
                "audio_streaming": v.get("audio_streaming").cloned().unwrap_or(Value::Null),
                "session": v.get("session").cloned().unwrap_or(Value::Null),
                "stream": v.get("stream").cloned().unwrap_or(Value::Null),
                "games": v.get("games").cloned().unwrap_or(Value::Null),
            });
            out(json, &slice, render_sessions);
            Ok(())
        }
        // `/status` has no device names. `/local/summary` is the one endpoint that names the client.
        "summary" => {
            let v = Client::connect(None)?.get("/api/v1/local/summary")?;
            out(json, &v, render_summary);
            Ok(())
        }
        "stop-session" => {
            let v = Client::connect(None)?.delete("/api/v1/session")?;
            out(json, &v, |_| println!("session stopped"));
            Ok(())
        }
        "end-game" => {
            let v = Client::connect(None)?.post("/api/v1/game/end", &json!({}))?;
            out(json, &v, |_| println!("game ended"));
            Ok(())
        }
        "pair" => pair(rest, json),
        "pending" => {
            let v = Client::connect(None)?.get("/api/v1/native/pending")?;
            out(json, &v, render_pending);
            Ok(())
        }
        "approve" => approve(rest, json),
        "deny" => {
            let id = one_id(rest, "deny")?;
            let v = Client::connect(None)?
                .post(&format!("/api/v1/native/pending/{id}/deny"), &json!({}))?;
            out(json, &v, move |_| println!("denied device {id}"));
            Ok(())
        }
        "pin" => {
            let [pin, uniqueid, fingerprint, peer_ip] = rest else {
                return Err(Failure::usage(
                    "pin: give PIN UNIQUEID FP PEER_IP exactly as `ctl status` shows them",
                ));
            };
            let body = json!({
                "pin": pin,
                "uniqueid": uniqueid,
                "fingerprint": fingerprint,
                "peer_ip": peer_ip,
            });
            let v = Client::connect(None)?.post("/api/v1/pair/pin", &body)?;
            out(json, &v, |_| println!("PIN submitted"));
            Ok(())
        }
        // Not an API call at all: a stub the console can verify with the token it already holds.
        // See `console_stub` — the point is that reading the 0600 token IS the proof.
        #[cfg(unix)]
        "console-url" => {
            let url = console_stub()?;
            out(json, &json!({ "url": url }), move |_| println!("{url}"));
            Ok(())
        }
        "clients" => {
            let c = Client::connect(None)?;
            // Both planes, labelled. A one-plane list is how "I unpaired it and it still connects" happens.
            let both = json!({
                "native": c.get("/api/v1/native/clients")?,
                "gamestream": c.get("/api/v1/clients")?,
            });
            out(json, &both, render_clients);
            Ok(())
        }
        "rename" => rename(rest, json),
        "unpair" => unpair(rest, json),
        "access" => access(rest, json),
        "display" => display(rest, json),
        "stats" => stats(rest, json),
        "watch" => {
            let kinds = flag_value(rest, "--kinds");
            let since = flag_value(rest, "--since")
                .map(|s| {
                    s.parse::<u64>().map_err(|_| {
                        Failure::usage("watch: --since takes an event sequence number")
                    })
                })
                .transpose()?;
            watch::run(kinds.as_deref(), since)
        }
        other => {
            print_usage();
            Err(Failure::usage(format!("unknown ctl verb '{other}'")))
        }
    }
}

fn pair(args: &[&str], json: bool) -> Result<()> {
    let c = Client::connect(None)?;
    match args.first().copied() {
        Some("arm") => {
            let mut body = json!({});
            if let Some(ttl) = num_flag(args, "--ttl")? {
                body["ttl_secs"] = json!(ttl);
            }
            if let Some(exp) = num_flag(args, "--expires-in")? {
                body["expires_in_secs"] = json!(exp);
            }
            if let Some(g) = preset_flag(args)? {
                body["grants"] = json!(g);
            }
            // `--fingerprint` binds the window to one device; without it any LAN peer can consume it.
            if let Some(fp) = flag_value(args, "--fingerprint") {
                body["fingerprint"] = json!(fp);
            }
            let v = c.post("/api/v1/native/pair/arm", &body)?;
            out(json, &v, render_pair);
            Ok(())
        }
        Some("disarm") => {
            let v = c.delete("/api/v1/native/pair")?;
            out(json, &v, |_| println!("pairing window closed"));
            Ok(())
        }
        Some("status") | None => {
            let v = c.get("/api/v1/native/pair")?;
            // GameStream PIN is a different route; fold it in so one command covers both planes.
            let mut v = v;
            if let Ok(gs) = c.get("/api/v1/pair") {
                v["pin_pending"] = gs.get("pin_pending").cloned().unwrap_or(json!(false));
                v["pending"] = gs.get("pending").cloned().unwrap_or(json!([]));
            }
            out(json, &v, render_pair);
            Ok(())
        }
        Some(other) => Err(Failure::usage(format!(
            "pair: expected arm | disarm | status, got '{other}'"
        ))),
    }
}

fn approve(args: &[&str], json: bool) -> Result<()> {
    let id = one_id(args, "approve")?;
    let mut body = json!({});
    if let Some(name) = flag_value(args, "--name") {
        body["name"] = json!(name);
    }
    if let Some(g) = preset_flag(args)? {
        body["grants"] = json!(g);
    }
    if let Some(exp) = num_flag(args, "--expires-in")? {
        body["expires_in_secs"] = json!(exp);
    }
    let v = Client::connect(None)?.post(&format!("/api/v1/native/pending/{id}/approve"), &body)?;
    out(json, &v, move |_| println!("approved device {id}"));
    Ok(())
}

fn rename(args: &[&str], json: bool) -> Result<()> {
    let (fp, name) = match (args.first(), args.get(1)) {
        (Some(fp), Some(name)) => (*fp, *name),
        _ => return Err(Failure::usage("rename: <fingerprint> <name>")),
    };
    let c = Client::connect(None)?;
    // Native first, GameStream on API error: a fingerprint lives in exactly one store.
    let native = c.patch(
        &format!("/api/v1/native/clients/{fp}"),
        &json!({ "name": name }),
    );
    let v = match native {
        Ok(v) => v,
        Err(e) if e.code == client::EXIT_API => {
            c.patch(&format!("/api/v1/clients/{fp}"), &json!({ "label": name }))?
        }
        Err(e) => return Err(e),
    };
    out(json, &v, move |_| println!("renamed {fp} to {name}"));
    Ok(())
}

fn unpair(args: &[&str], json: bool) -> Result<()> {
    let c = Client::connect(None)?;
    if args.contains(&"--all") {
        // `--all` is the only mass-destructive verb. JSON cannot prompt, so it needs `--yes`.
        if !args.contains(&"--yes") {
            if json {
                return Err(Failure::usage(
                    "unpair --all needs --yes in --json mode (it cannot prompt)",
                ));
            }
            if !confirm("Unpair EVERY device on both planes? [y/N] ") {
                return Err(Failure::usage("cancelled"));
            }
        }
        let both = json!({
            "native": c.delete("/api/v1/native/clients")?,
            "gamestream": c.delete("/api/v1/clients")?,
        });
        out(json, &both, |_| println!("all devices unpaired"));
        return Ok(());
    }
    let fp = args
        .first()
        .copied()
        .filter(|a| !a.starts_with("--"))
        .ok_or_else(|| Failure::usage("unpair: <fingerprint>, or --all"))?;
    let native = c.delete(&format!("/api/v1/native/clients/{fp}"));
    let v = match native {
        Ok(v) => v,
        Err(e) if e.code == client::EXIT_API => c.delete(&format!("/api/v1/clients/{fp}"))?,
        Err(e) => return Err(e),
    };
    out(json, &v, move |_| println!("unpaired {fp}"));
    Ok(())
}

fn access(args: &[&str], json: bool) -> Result<()> {
    let (fp, preset) = match (args.first(), args.get(1)) {
        (Some(fp), Some(p)) => (*fp, *p),
        _ => {
            return Err(Failure::usage(
                "access: <fingerprint> <full|controller|view>",
            ))
        }
    };
    // Presets only. A bitmask on argv is how a digit-wrong grant hands a device the keyboard.
    let grants = grants_for(preset)?;
    let v = Client::connect(None)?.patch(
        &format!("/api/v1/native/clients/{fp}"),
        &json!({ "grants": grants }),
    )?;
    out(json, &v, move |_| println!("{fp}: access set to {preset}"));
    Ok(())
}

/// Virtual-display policy and live heads, in one answer.
///
/// Two GETs because the API keeps them apart: `/display/settings` is stored policy,
/// `/display/state` is this second. One verb so they are not shown at two instants.
///
/// `displays` is empty on wlroots: the registry passes portal-fd heads through rather
/// than owning them, so it never lists them. Read `[]` as "this host does not track
/// them", never as "there are none".
fn display(args: &[&str], json: bool) -> Result<()> {
    let c = Client::connect(None)?;
    match args.first().copied() {
        Some("preset") => {
            let id = args
                .get(1)
                .copied()
                .ok_or_else(|| Failure::usage("display preset: <ID> (see `ctl display`)"))?;
            let v = display_apply_preset(&c, id)?;
            out(json, &v, move |_| println!("display preset set to {id}"));
            Ok(())
        }
        Some("release") => {
            // Slot optional; omit it to release every KEPT head. Active heads are `stop-session`.
            let mut body = json!({});
            if let Some(slot) = args.get(1) {
                let n: u32 = slot
                    .parse()
                    .map_err(|_| Failure::usage("display release: SLOT must be a number"))?;
                body["slot"] = json!(n);
            }
            let v = c.post("/api/v1/display/release", &body)?;
            out(json, &v, |v| {
                println!(
                    "released {} kept display(s)",
                    v["released"].as_i64().unwrap_or(0)
                )
            });
            Ok(())
        }
        None | Some("status") => {
            let mut v = c.get("/api/v1/display/settings")?;
            // No vdisplay backend: empty `displays` rather than failing the whole verb.
            let live = c
                .get("/api/v1/display/state")
                .map(|s| s["displays"].clone())
                .unwrap_or(Value::Null);
            v["displays"] = if live.is_array() { live } else { json!([]) };
            out(json, &v, render_display);
            Ok(())
        }
        Some(other) => Err(Failure::usage(format!(
            "display: expected status | preset | release, got '{other}'"
        ))),
    }
}

/// Switch stored policy to `id`, keeping every axis a preset does not own.
///
/// `PUT /display/settings` replaces the whole object, so this reads first and edits:
/// `capture_monitor` and the Windows axes are not preset behaviour, and dropping them
/// would swap the streamed screen.
///
/// A saved custom preset has no apply route; applying it writes a `Custom` policy with
/// its saved fields so both kinds of id work.
fn display_apply_preset(c: &Client, id: &str) -> Result<Value> {
    let state = c.get("/api/v1/display/settings")?;
    let policy = policy_with_preset(&state, id)?;
    c.put("/api/v1/display/settings", &policy)
}

/// Policy edit, split from HTTP so tests can feed a settings body without a host.
fn policy_with_preset(state: &Value, id: &str) -> Result<Value> {
    let mut policy = state["settings"].clone();
    if !policy.is_object() {
        return Err(Failure::api("display: the host returned no stored policy"));
    }

    let custom = state["custom_presets"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|p| p["id"].as_str() == Some(id))
        .cloned();

    if let Some(p) = custom {
        policy["preset"] = json!("custom");
        for (k, val) in p["fields"].as_object().into_iter().flatten() {
            policy[k.as_str()] = val.clone();
        }
        // `game_session` is the one orthogonal axis a saved preset carries.
        if let Some(gs) = p.get("game_session") {
            policy["game_session"] = gs.clone();
        }
    } else {
        let known = state["presets"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|p| p["id"].as_str() == Some(id));
        if !known {
            // Built-ins and saved presets share one id space; only the host knows the saved half.
            return Err(Failure::usage(format!(
                "display preset: no preset '{id}' — run `ctl display` for the list"
            )));
        }
        policy["preset"] = json!(id);
    }
    Ok(policy)
}

fn render_display(v: &Value) {
    let cur = v["settings"]["preset"].as_str().unwrap_or("custom");
    println!("preset    {cur}");
    let eff = &v["effective"];
    println!(
        "policy    topology {} · identity {} · conflict {} · max {}",
        eff["topology"].as_str().unwrap_or("—"),
        eff["identity"].as_str().unwrap_or("—"),
        eff["mode_conflict"].as_str().unwrap_or("—"),
        eff["max_displays"].as_i64().unwrap_or(0)
    );
    for p in v["presets"].as_array().into_iter().flatten() {
        let id = p["id"].as_str().unwrap_or("");
        println!(
            "  {}{:<16} {}",
            if id == cur { "*" } else { " " },
            id,
            trunc(p["summary"].as_str().unwrap_or(""), 60)
        );
    }
    for p in v["custom_presets"].as_array().into_iter().flatten() {
        println!(
            "  {:<17} {} (saved)",
            p["id"].as_str().unwrap_or(""),
            trunc(p["name"].as_str().unwrap_or(""), 50)
        );
    }
    let live = v["displays"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    if live.is_empty() {
        println!("live      none");
        return;
    }
    for d in live {
        println!(
            "live      slot {} · {} · {} · {} session(s){}",
            d["slot"].as_i64().unwrap_or(0),
            d["mode"].as_str().unwrap_or("—"),
            d["state"].as_str().unwrap_or("—"),
            d["sessions"].as_i64().unwrap_or(0),
            d["client"]
                .as_str()
                .map(|c| format!(" · {}", trunc(c, 24)))
                .unwrap_or_default()
        );
    }
}

/// Live stream readout: mode, codec, encoder target; frame timings only while armed.
///
/// `/status` is free. Per-frame drops and stage percentiles exist only while a
/// performance capture is armed — that is when the loops emit samples. This verb never
/// arms as a side effect of being read: arming writes a recording on `stop`, and the
/// capture is one host-wide slot the console also drives.
fn stats(args: &[&str], json: bool) -> Result<()> {
    let c = Client::connect(None)?;
    match args.first().copied() {
        Some("record") => match args.get(1).copied() {
            Some("start") => {
                let v = c.post("/api/v1/stats/capture/start", &json!({}))?;
                out(json, &v, |_| println!("capture armed"));
                Ok(())
            }
            Some("stop") => {
                let v = c.post("/api/v1/stats/capture/stop", &json!({}))?;
                out(json, &v, |v| {
                    // 204 / null body: nothing was recording.
                    match v["id"].as_str() {
                        Some(id) => println!("capture saved as {id}"),
                        None => println!("nothing was recording"),
                    }
                });
                Ok(())
            }
            _ => Err(Failure::usage("stats record: expected start | stop")),
        },
        None | Some("status") => {
            let status = c.get("/api/v1/status")?;
            let capture = c
                .get("/api/v1/stats/capture/status")
                .unwrap_or_else(|_| json!({ "armed": false }));
            // Live capture holds the whole series (up to 5400 samples). Take the tail; callers render one row.
            let mut sample = Value::Null;
            let mut meta = Value::Null;
            if capture["armed"].as_bool().unwrap_or(false) {
                if let Ok(live) = c.get("/api/v1/stats/capture/live") {
                    meta = live["meta"].clone();
                    sample = live["samples"]
                        .as_array()
                        .and_then(|s| s.last())
                        .cloned()
                        .unwrap_or(Value::Null);
                }
            }
            let v = json!({
                "video_streaming": status["video_streaming"],
                "active_sessions": status["active_sessions"],
                "session": status["session"],
                "sessions": status["sessions"],
                "stream": status["stream"],
                "games": status["games"],
                "capture": capture,
                "sample": sample,
                "meta": meta,
            });
            out(json, &v, render_stats);
            Ok(())
        }
        Some(other) => Err(Failure::usage(format!(
            "stats: expected status | record, got '{other}'"
        ))),
    }
}

/// Stage duration in a unit that keeps both ends visible.
/// Encode is ~2300 µs and `send` ~15 µs: ms flattens send to `0.0`, µs makes encode five digits.
fn dur_us(us: f64) -> String {
    if us >= 1000.0 {
        format!("{:.1} ms", us / 1000.0)
    } else {
        format!("{} µs", us.round() as i64)
    }
}

/// Per session: other sessions on its client address, then the last closed link-health minute —
/// the same counters as the host log's `link health` line, silent until a full minute has run.
fn render_link(v: &Value) {
    let Some(sessions) = v["sessions"].as_array() else {
        return;
    };
    for s in sessions {
        if let Some(others) = s["shared_path_with"].as_array() {
            let ids: Vec<String> = others.iter().map(|o| o.to_string()).collect();
            println!(
                "path      session {} shares its client address with session {}",
                s["id"],
                ids.join(", ")
            );
        }
        let l = &s["link"];
        if l.is_null() {
            continue;
        }
        let n = |k: &str| l[k].as_i64().unwrap_or(0);
        println!(
            "link      session {} · {} report windows, {} with loss (max {:.2} %)",
            n("session_id"),
            n("windows"),
            n("loss_windows"),
            l["loss_max_ppm"].as_f64().unwrap_or(0.0) / 10_000.0,
        );
        // Recovery is what a freeze costs; the bands say what the link was asked to carry.
        println!(
            "          {} keyframe asks · {} IDR · {} RFI ({} declined) · {} anchor · {} intra refresh",
            n("keyframe_req"),
            n("idr"),
            n("rfi"),
            n("rfi_declined"),
            n("anchor_p"),
            n("intra_refresh"),
        );
        println!(
            "          FEC {}–{} % · {:.1}–{:.1} Mbps target, {} retargets · {:.1} Mbps sent",
            n("fec_min_pct"),
            n("fec_max_pct"),
            n("abr_min_kbps") as f64 / 1000.0,
            n("abr_max_kbps") as f64 / 1000.0,
            n("retargets"),
            n("egress_kbps") as f64 / 1000.0,
        );
    }
}

fn render_stats(v: &Value) {
    let s = &v["stream"];
    if s.is_null() {
        println!("stream    nothing is streaming");
    } else {
        println!(
            "stream    {}x{}@{} {}",
            s["width"].as_i64().unwrap_or(0),
            s["height"].as_i64().unwrap_or(0),
            s["fps"].as_i64().unwrap_or(0),
            s["codec"].as_str().unwrap_or("—")
        );
        // Encoder target (ABR), not throughput. On a still screen they differ by an order of magnitude.
        println!(
            "target    {:.1} Mbps",
            s["bitrate_kbps"].as_f64().unwrap_or(0.0) / 1000.0
        );
        if let Some(ms) = s["time_to_first_frame_ms"].as_i64() {
            println!("bring-up  {ms} ms to first frame");
        }
    }
    render_link(v);
    if let Some(backend) = v["meta"]["encoder_backend"].as_str() {
        println!(
            "encoder   {backend}{}",
            v["meta"]["gpu"]
                .as_str()
                .map(|g| format!(" · {g}"))
                .unwrap_or_default()
        );
    }
    if !v["capture"]["armed"].as_bool().unwrap_or(false) {
        println!("capture   idle — run `ctl stats record start` for frame timings");
        return;
    }
    println!(
        "capture   recording · {} samples",
        v["capture"]["sample_count"].as_i64().unwrap_or(0)
    );
    let p = &v["sample"];
    if p.is_null() {
        return;
    }
    println!("sent      {:.1} Mbps", p["mbps"].as_f64().unwrap_or(0.0));
    // `fps` is new frames; `repeat_fps` is the last one re-sent. A still desktop reads 0 new fps while healthy.
    let new_fps = p["fps"].as_f64().unwrap_or(0.0);
    let repeat_fps = p["repeat_fps"].as_f64().unwrap_or(0.0);
    println!("frames    {new_fps:.1} fps new · {repeat_fps:.1} fps repeated");
    if new_fps < 1.0 && repeat_fps > 1.0 {
        println!("          (nothing on screen is changing — the last frame is being repeated)");
    }
    // A counter this path cannot see is absent from the sample; print only what was measured.
    let loss: Vec<String> = [
        ("frames_dropped", "frames dropped"),
        ("send_dropped", "send drops"),
        ("packets_dropped", "packets lost"),
        ("fec_recovered", "recovered by FEC"),
    ]
    .iter()
    .filter_map(|(k, label)| p[*k].as_i64().map(|n| format!("{n} {label}")))
    .collect();
    if !loss.is_empty() {
        println!("loss      {}", loss.join(" · "));
    }
    if let (Some(p50), Some(p99)) = (p["host_p50_us"].as_f64(), p["host_p99_us"].as_f64()) {
        println!(
            "host      {} p50 · {} p99 (capture to sent)",
            dur_us(p50),
            dur_us(p99)
        );
    }
    if let Some(rtt) = p["rtt_us"].as_f64() {
        println!("rtt       {}", dur_us(rtt));
    }
    for st in p["stages"].as_array().into_iter().flatten() {
        println!(
            "  {:<12} p50 {} · p99 {}",
            st["name"].as_str().unwrap_or("—"),
            dur_us(st["p50_us"].as_f64().unwrap_or(0.0)),
            dur_us(st["p99_us"].as_f64().unwrap_or(0.0))
        );
    }
}

/// Write a one-shot login page for the browser to open, and return its `file://` URL.
///
/// **What is being trusted.** The ticket is keyed by the **mgmt token**: a 0600 file inside the
/// 0700 config dir, readable only by the uid the host runs as. Whoever can read it can already
/// drive the whole admin API directly — it is the credential the console's own proxy presents —
/// so letting them skip a password they could simply read widens nothing. A LAN visitor without a
/// ticket still meets the login page.
///
/// **Why a file and not the URL.** `/proc/<pid>/cmdline` is world-readable, so a ticket passed to
/// the browser as an argument is legible to every other local user for its whole lifetime. The
/// ticket therefore lives in a 0600 file under `$XDG_RUNTIME_DIR` — per-user, 0700, wiped at
/// logout — and only its PATH reaches argv. No runtime dir means no private place to put it, so
/// this fails and the caller opens the bare console URL instead.
///
/// TTL and single-use are the console's to enforce, so a stub left behind by an earlier launch is
/// already spent.
#[cfg(unix)]
fn console_stub() -> Result<String> {
    let dir = pf_paths::config_dir();
    let token = crate::mgmt_token::read_persisted(&dir).ok_or_else(|| {
        Failure::unreachable(format!(
            "no management token in {} — the console shares this file, so without it there is \
             nothing for a handoff to prove. Start the host once and retry.",
            dir.join("mgmt-token").display()
        ))
    })?;
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut raw = [0u8; 16];
    rand::RngCore::fill_bytes(&mut rand::rng(), &mut raw);
    let nonce = hex::encode(raw);
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").ok_or_else(|| {
        Failure::unreachable(
            "no XDG_RUNTIME_DIR — there is nowhere to put a ticket that another local user cannot \
             read. Open the console and log in instead."
                .to_string(),
        )
    })?;
    let ticket = handoff_ticket(&token, ts, &nonce)?;
    // The console's own port, not the mgmt one. It is not published anywhere the way
    // `mgmt-endpoint` is, so the documented default stands until somebody moves it.
    let target = format!("https://localhost:47992/_auth/handoff?t={ticket}");
    write_stub(&std::path::Path::new(&runtime).join("punktfunk"), &target)
}

/// The signed half: `<unix-seconds>.<nonce>.<HMAC-SHA256>` over `pf-console-handoff:v1:ts:nonce`.
///
/// Kept alone because it is the cross-language contract — `handoffMessage` in
/// `web/server/util/handoff.ts` builds the same string, and the console recomputes this MAC with
/// its own copy of the token. The nonce is what keeps two launches in the same second from
/// colliding in the console's replay set.
#[cfg(unix)]
fn handoff_ticket(token: &str, ts: u64, nonce: &str) -> Result<String> {
    use hmac::{Hmac, KeyInit, Mac};
    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(token.as_bytes())
        .map_err(|e| Failure::api(format!("key the handoff HMAC: {e}")))?;
    mac.update(format!("pf-console-handoff:v1:{ts}:{nonce}").as_bytes());
    Ok(format!(
        "{ts}.{nonce}.{}",
        hex::encode(mac.finalize().into_bytes())
    ))
}

/// Put `target` behind a 0600 page in `dir`, and return the `file://` URL of that page.
///
/// The directory is forced to 0700 because it is the only thing standing between the ticket and
/// another local user; the file is opened 0600 rather than chmod'ed afterwards, since the window
/// between create and chmod is the exposure this whole path exists to close.
#[cfg(unix)]
fn write_stub(dir: &std::path::Path, target: &str) -> Result<String> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let path = dir.join("console-open.html");
    let io = |e: std::io::Error| Failure::api(format!("write {}: {e}", path.display()));
    std::fs::create_dir_all(dir).map_err(io)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).map_err(io)?;
    // No escaping: every byte of the ticket is `[0-9a-f.]` by construction, so it is inert in an
    // attribute and in a href. The link is the fallback for a browser that ignores refresh.
    let page = format!(
        "<!doctype html>\n<meta charset=\"utf-8\">\n<title>Punktfunk console</title>\n\
         <meta http-equiv=\"refresh\" content=\"0;url={target}\">\n\
         <p><a href=\"{target}\">Open the Punktfunk console</a>\n"
    );
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)
        .map_err(io)?;
    f.write_all(page.as_bytes()).map_err(io)?;
    Ok(format!("file://{}", path.display()))
}

fn grants_for(preset: &str) -> Result<u32> {
    match preset {
        "full" => Ok(GRANT_PRESET_FULL),
        "controller" => Ok(GRANT_PRESET_CONTROLLER_ONLY),
        "view" => Ok(GRANT_PRESET_VIEW_ONLY),
        other => Err(Failure::usage(format!(
            "unknown access preset '{other}' (want full | controller | view)"
        ))),
    }
}

fn flag_value(args: &[&str], flag: &str) -> Option<String> {
    let i = args.iter().position(|a| *a == flag)?;
    args.get(i + 1).map(|s| (*s).to_string())
}

fn num_flag(args: &[&str], flag: &str) -> Result<Option<u64>> {
    match flag_value(args, flag) {
        None => Ok(None),
        Some(v) => v
            .parse()
            .map(Some)
            .map_err(|_| Failure::usage(format!("{flag} takes a number of seconds, got '{v}'"))),
    }
}

fn preset_flag(args: &[&str]) -> Result<Option<u32>> {
    flag_value(args, "--preset")
        .map(|p| grants_for(&p))
        .transpose()
}

/// Pending-device id: always the first argument, never the first number on the line.
/// `--expires-in 3600` must not become "approve device 3600".
fn one_id(args: &[&str], verb: &str) -> Result<u32> {
    let raw = args
        .first()
        .filter(|a| !a.starts_with("--"))
        .ok_or_else(|| Failure::usage(format!("{verb}: <id> first, from `ctl pending`")))?;
    raw.parse()
        .map_err(|_| Failure::usage(format!("{verb}: '{raw}' is not a pending-device id")))
}

fn confirm(prompt: &str) -> bool {
    use std::io::Write as _;
    print!("{prompt}");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).is_ok() && matches!(line.trim(), "y" | "Y" | "yes")
}

/// Versioned envelope in `--json`; terse table otherwise. Nothing we ship parses the human half.
fn out(json: bool, v: &Value, human: impl FnOnce(&Value)) {
    if json {
        println!("{}", json!({ "v": SCHEMA_VERSION, "data": v }));
    } else {
        human(v);
    }
}

fn render_status(v: &Value) {
    let streaming = v["video_streaming"].as_bool().unwrap_or(false);
    println!("host      {}", if streaming { "streaming" } else { "idle" });
    println!("sessions  {}", v["active_sessions"].as_i64().unwrap_or(0));
    println!(
        "paired    {} native, {} gamestream",
        v["native_paired_clients"].as_i64().unwrap_or(0),
        v["paired_clients"].as_i64().unwrap_or(0)
    );
    if v["pin_pending"].as_bool().unwrap_or(false) {
        println!("pin       PENDING — run `ctl pair status` for the ceremony identity");
    }
    render_games(v);
}

fn render_sessions(v: &Value) {
    println!("sessions  {}", v["active_sessions"].as_i64().unwrap_or(0));
    if let Some(s) = v["session"].as_object() {
        println!(
            "mode      {}x{} @ {}",
            s.get("width").and_then(Value::as_i64).unwrap_or(0),
            s.get("height").and_then(Value::as_i64).unwrap_or(0),
            s.get("fps").and_then(Value::as_i64).unwrap_or(0)
        );
    }
    render_games(v);
}

fn render_summary(v: &Value) {
    println!("version   {}", v["version"].as_str().unwrap_or("—"));
    println!(
        "state     {}",
        if v["video_streaming"].as_bool().unwrap_or(false) {
            match v["audio_streaming"].as_bool().unwrap_or(false) {
                true => "streaming video + audio",
                false => "streaming video",
            }
        } else {
            "idle"
        }
    );
    if let Some(name) = v["client_name"].as_str() {
        println!("client    {name}");
    }
    if let Some(s) = v["session"].as_object() {
        println!(
            "mode      {}x{} @ {}",
            s.get("width").and_then(Value::as_i64).unwrap_or(0),
            s.get("height").and_then(Value::as_i64).unwrap_or(0),
            s.get("fps").and_then(Value::as_i64).unwrap_or(0)
        );
    }
    println!(
        "paired    {} native, {} gamestream",
        v["native_paired_clients"].as_i64().unwrap_or(0),
        v["paired_clients"].as_i64().unwrap_or(0)
    );
    let waiting = v["pending_approvals"].as_i64().unwrap_or(0);
    if waiting > 0 {
        println!("pending   {waiting} awaiting approval");
    }
    // Another Moonlight-compatible host on this box binds the same ports.
    for c in v["conflicts"].as_array().into_iter().flatten() {
        println!("conflict  {}", c.as_str().unwrap_or(""));
    }
}

fn render_games(v: &Value) {
    let Some(games) = v["games"].as_array().filter(|g| !g.is_empty()) else {
        return;
    };
    println!("\nGAME                           CLIENT              PLANE       STATE");
    for g in games {
        println!(
            "{:<30} {:<19} {:<11} {}",
            trunc(g["title"].as_str().unwrap_or("(desktop)"), 30),
            trunc(g["client"].as_str().unwrap_or("—"), 19),
            g["plane"].as_str().unwrap_or("—"),
            g["state"].as_str().unwrap_or("—"),
        );
    }
}

fn render_pending(v: &Value) {
    let Some(rows) = v.as_array().filter(|r| !r.is_empty()) else {
        println!("no devices waiting for approval");
        return;
    };
    // Claimed name next to the fingerprint tail: the name is asserted, the tail is not.
    println!("ID    NAME                      FINGERPRINT  AGE      ACCESS");
    for r in rows {
        println!(
            "{:<5} {:<25} {:<12} {:<8} {}",
            r["id"].as_i64().unwrap_or(-1),
            trunc(r["name"].as_str().unwrap_or("(unnamed)"), 25),
            tail(r["fingerprint"].as_str().unwrap_or("")),
            format!("{}s", r["age_secs"].as_i64().unwrap_or(0)),
            r["access_level"].as_str().unwrap_or("—"),
        );
    }
    println!("\napprove with `ctl approve <ID>`, refuse with `ctl deny <ID>`");
}

fn render_clients(v: &Value) {
    println!("PLANE       NAME                      FINGERPRINT  ACCESS      EXPIRES");
    for r in v["native"].as_array().into_iter().flatten() {
        println!(
            "{:<11} {:<25} {:<12} {:<11} {}",
            "native",
            trunc(r["name"].as_str().unwrap_or("(unnamed)"), 25),
            tail(r["fingerprint"].as_str().unwrap_or("")),
            r["access_level"].as_str().unwrap_or("—"),
            match r["expires_unix"].as_i64() {
                None => "permanent".to_string(),
                Some(t) => format!("unix {t}"),
            }
        );
    }
    for r in v["gamestream"].as_array().into_iter().flatten() {
        // GameStream devices are pinned certificates: no grants, no expiry.
        println!(
            "{:<11} {:<25} {:<12} {:<11} —",
            "gamestream",
            trunc(r["label"].as_str().unwrap_or("(unnamed)"), 25),
            tail(r["fingerprint"].as_str().unwrap_or("")),
            "—",
        );
    }
}

fn render_pair(v: &Value) {
    println!(
        "native    {}",
        match (
            v["enabled"].as_bool().unwrap_or(false),
            v["armed"].as_bool().unwrap_or(false)
        ) {
            (false, _) => "the native plane is not running".to_string(),
            (true, false) => "disarmed".to_string(),
            (true, true) => match v["expires_in_secs"].as_i64() {
                Some(s) => format!("ARMED, {s}s left"),
                None => "ARMED".to_string(),
            },
        }
    );
    if let Some(pin) = v["pin"].as_str() {
        println!("pin       {pin}  — enter this on the device");
    }
    if v["pin_pending"].as_bool().unwrap_or(false) {
        println!("moonlight select one waiting ceremony, then submit its exact identity:");
        for c in v["pending"].as_array().into_iter().flatten() {
            println!(
                "          ctl pin <PIN> {} {} {}",
                c["uniqueid"].as_str().unwrap_or("?"),
                c["fingerprint"].as_str().unwrap_or("?"),
                c["peer_ip"].as_str().unwrap_or("?")
            );
        }
    }
    println!("paired    {}", v["paired_clients"].as_i64().unwrap_or(0));
}

/// Last 10 hex characters: enough to tell devices apart in a compact list.
fn tail(fp: &str) -> String {
    match fp.len() {
        0 => "—".to_string(),
        n if n <= 10 => fp.to_string(),
        n => format!("…{}", &fp[n - 10..]),
    }
}

fn trunc(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max.saturating_sub(1)).collect::<String>() + "…"
}

fn print_usage() {
    // `{USAGE}` not a format string: the `watch` example contains JSON braces.
    const USAGE: &str = r#"punktfunk-host ctl — operator control over the local management API

USAGE:
    punktfunk-host ctl <VERB> [ARGS] [--json]

STATE
    status                       host state, session count, paired counts
    sessions                     the active session(s) and any launched game
    summary                      one call for a status surface: host version, what is streaming,
                                 the streaming client's name, paired counts, and any conflicting
                                 host on this box. The only verb that names a connected device.
    watch [--kinds K,..] [--since N]
                                 the host event stream as line-JSON on stdout, one object per
                                 line; reconnects by itself and emits {"kind":"ctl.resync"}
                                 when it fell off the catch-up ring

PAIRING
    pair status                  is a pairing window open, and is a PIN waiting
    pair arm [--ttl S] [--expires-in S] [--preset P] [--fingerprint FP]
                                 open a native pairing window (--fingerprint binds it to ONE device)
    pair disarm                  close it
    pending                      devices knocking, awaiting approval
    approve <ID> [--name N] [--preset P] [--expires-in S]
    deny <ID>
    pin <PIN> <UNIQUEID> <FP> <PEER_IP>
                                 submit to the exact ceremony shown by `status`

DEVICES
    clients                      paired devices on both planes
    rename <FP> <NAME>
    access <FP> <full|controller|view>
    unpair <FP> | unpair --all [--yes]

CONSOLE
    console-url                  write a one-shot login page under $XDG_RUNTIME_DIR and print its
                                 file:// URL, so a browser opened at it lands already logged in.
                                 The ticket stays out of argv, where every local user can read it.

DISPLAYS
    display                      the virtual-display policy, every preset, and the live displays.
                                 `displays` is always empty on wlroots — the registry passes those
                                 through rather than owning them, so read it as "not tracked here".
    display preset <ID>          switch the policy to a preset — a built-in id or a saved one.
                                 Reads the stored policy and edits it, so the axes a preset does
                                 not own (the streamed screen, the experimental Windows ones)
                                 survive the switch.
    display release [SLOT]       tear down KEPT displays now, so a physical-screen user gets their
                                 screen back without waiting out the linger. Omit SLOT for all.
                                 Never touches a display that is actively streaming.

STATS
    stats                        the live stream: mode, codec and the adaptive bitrate, plus frame
                                 timings and drops while a capture is recording
    stats record start|stop      arm or disarm the performance capture. `stop` writes the recording
                                 to disk; the capture is one host-wide slot the web console shares.

OPTIONS
    --json                       versioned JSON on stdout (the contract; the tables are for humans)

EXIT CODES
    0 ok   1 the host refused   2 usage   3 no host reachable   4 certificate pin mismatch

The token and the certificate pin are read from the host's config directory; neither is ever
accepted on the command line or from the environment."#;
    eprintln!("{USAGE}");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pins the cross-language contract. The expected MAC was computed outside this code (openssl
    /// and python agree), so a change to the signed string fails here instead of silently handing
    /// the console tickets it will reject.
    #[cfg(unix)]
    #[test]
    fn the_ticket_signs_what_the_console_verifies() {
        let t = handoff_ticket("test-token", 1_700_000_000, "abc123").unwrap();
        assert_eq!(
            t,
            "1700000000.abc123.754d195691c7b24f4a6b8966541deca3c958919e7d33da9e0d4e3acfebf6df2c"
        );
    }

    /// The ticket must never be readable by another local user — that is the whole reason the URL
    /// stopped being passed as an argument.
    #[cfg(unix)]
    #[test]
    fn the_stub_is_private_and_overwrites_cleanly() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("punktfunk");

        let url = write_stub(&dir, "https://localhost:47992/_auth/handoff?t=1.a.b").unwrap();
        let path = dir.join("console-open.html");
        assert_eq!(url, format!("file://{}", path.display()));
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let page = std::fs::read_to_string(&path).unwrap();
        assert!(page.contains("t=1.a.b"), "the ticket reaches the browser");

        // A second launch must not leave the previous ticket behind in the tail of the file.
        write_stub(&dir, "https://localhost:47992/_auth/handoff?t=2.c.d").unwrap();
        let page = std::fs::read_to_string(&path).unwrap();
        assert!(!page.contains("t=1.a.b"), "the spent ticket is gone");
    }

    #[test]
    fn presets_map_to_the_hosts_own_masks() {
        assert_eq!(grants_for("full").unwrap(), GRANT_PRESET_FULL);
        assert_eq!(
            grants_for("controller").unwrap(),
            GRANT_PRESET_CONTROLLER_ONLY
        );
        assert_eq!(grants_for("view").unwrap(), GRANT_PRESET_VIEW_ONLY);
        assert_eq!(grants_for("fulll").unwrap_err().code, client::EXIT_USAGE);
    }

    #[test]
    fn fingerprint_tails_stay_short_and_never_panic_on_multibyte() {
        assert_eq!(tail(""), "—");
        assert_eq!(tail("abc"), "abc");
        assert_eq!(tail(&"a".repeat(64)), format!("…{}", "a".repeat(10)));
        assert_eq!(trunc("ünïcödé title", 5), "ünïc…");
    }

    #[test]
    fn flags_are_read_positionlessly() {
        let args = ["arm", "--ttl", "120", "--preset", "view"];
        assert_eq!(num_flag(&args, "--ttl").unwrap(), Some(120));
        assert_eq!(preset_flag(&args).unwrap(), Some(GRANT_PRESET_VIEW_ONLY));
        assert_eq!(num_flag(&args, "--expires-in").unwrap(), None);
        assert_eq!(
            num_flag(&["--ttl", "soon"], "--ttl").unwrap_err().code,
            client::EXIT_USAGE
        );
    }

    #[test]
    fn approve_takes_an_id_and_never_guesses() {
        assert_eq!(one_id(&["7", "--name", "tv"], "approve").unwrap(), 7);
        // A flag value must never be the id.
        assert!(one_id(&["--expires-in", "3600"], "approve").is_err());
        assert!(one_id(&["newest"], "approve").is_err());
        assert!(one_id(&[], "approve").is_err());
    }

    /// Fixture: stored policy with axes no preset owns, one built-in, one saved.
    fn display_state() -> Value {
        json!({
            "settings": {
                "version": 1,
                "preset": "default",
                "topology": "auto",
                "max_displays": 4,
                "game_session": "auto",
                "capture_monitor": "DP-2",
                "ddc_power_off": true,
            },
            "presets": [{ "id": "gaming-rig", "summary": "…" }],
            "custom_presets": [{
                "id": "p-couch",
                "name": "Couch",
                "game_session": "dedicated",
                "fields": { "topology": "exclusive", "max_displays": 2 },
            }],
        })
    }

    #[test]
    fn a_preset_switch_keeps_the_axes_no_preset_owns() {
        let p = policy_with_preset(&display_state(), "gaming-rig").unwrap();
        assert_eq!(p["preset"], "gaming-rig");
        // `capture_monitor` is not preset behaviour. A whole-object PUT from the verb args would drop it.
        assert_eq!(p["capture_monitor"], "DP-2");
        assert_eq!(p["ddc_power_off"], true);
    }

    #[test]
    fn a_saved_preset_is_applied_as_a_custom_policy_carrying_its_fields() {
        // Saved presets have no apply route: the API expects `custom` plus the fields.
        let p = policy_with_preset(&display_state(), "p-couch").unwrap();
        assert_eq!(p["preset"], "custom");
        assert_eq!(p["topology"], "exclusive");
        assert_eq!(p["max_displays"], 2);
        assert_eq!(p["game_session"], "dedicated");
        assert_eq!(p["capture_monitor"], "DP-2");
    }

    #[test]
    fn stage_durations_keep_the_small_stages_visible() {
        assert_eq!(dur_us(15.0), "15 µs");
        assert_eq!(dur_us(0.0), "0 µs");
        assert_eq!(dur_us(999.0), "999 µs");
        assert_eq!(dur_us(2321.0), "2.3 ms");
    }

    #[test]
    fn an_unknown_preset_is_refused_rather_than_stored() {
        assert!(policy_with_preset(&display_state(), "hologram").is_err());
        // Empty settings is the host's answer, not a preset the caller can fix.
        assert!(policy_with_preset(&json!({}), "gaming-rig").is_err());
    }
}
