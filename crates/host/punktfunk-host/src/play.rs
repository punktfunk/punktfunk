//! `punktfunk-host play`: a library title inside Steam's process tree. The Steam entry the
//! Decky plugin synced runs it, so the game is Steam's: its name, art and Steam Input layout,
//! and Steam's Exit ends it. `play` asks the running host for the launch (`POST
//! /game/handoff`), starts the command it answers as its own child, reports the child's start
//! and exit, and stays until the host no longer lists the title. It speaks to the host as `ctl`
//! does: the host's token, sent only once the certificate pin holds.

use crate::ctl::client::{read_capped, Client, Failure, EXIT_UNREACHABLE};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::Duration;

/// How often `play` asks whether the host still lists the title.
const POLL: Duration = Duration::from_secs(1);

/// What Steam and the wrapper set for the entry's launch.
struct Launch {
    /// `SteamGameId`: the entry's 64-bit game id.
    gameid: String,
    /// `SDL_GAMECONTROLLER_IGNORE_DEVICES` and its `_EXCEPT`.
    sdl_ignore: String,
    sdl_ignore_except: String,
    /// Steam's `LD_LIBRARY_PATH` and `LD_PRELOAD`, which the wrapper kept for the game.
    ld_library_path: Option<String>,
    ld_preload: Option<String>,
}

pub fn main() -> ! {
    let var = |k: &str| std::env::var(k).ok();
    let launch = Launch {
        gameid: var("SteamGameId").unwrap_or_default(),
        sdl_ignore: var("SDL_GAMECONTROLLER_IGNORE_DEVICES").unwrap_or_default(),
        sdl_ignore_except: var("SDL_GAMECONTROLLER_IGNORE_DEVICES_EXCEPT").unwrap_or_default(),
        ld_library_path: var("PF_STEAM_LD_LIBRARY_PATH"),
        ld_preload: var("PF_STEAM_LD_PRELOAD"),
    };
    let code = match Client::connect(None) {
        Ok(client) => run(&client, &launch),
        Err(f) => failed(&f),
    };
    std::process::exit(code)
}

/// The launch, start to the host letting go: the child's exit code, or the failure's.
fn run(client: &Client, launch: &Launch) -> i32 {
    let ask = json!({
        "steam_gameid": launch.gameid,
        "sdl_ignore": launch.sdl_ignore,
        "sdl_ignore_except": launch.sdl_ignore_except,
    });
    let handed = match call(client, "/api/v1/game/handoff", &ask) {
        Ok(v) => v,
        Err(code) => return code,
    };
    let app = handed["app_id"].as_str().unwrap_or_default();
    // A title the host already runs comes with no command: wait for it to end.
    let Some(command) = handed["command"]
        .as_str()
        .filter(|_| handed["running"] != true)
    else {
        wait_until_gone(client, app);
        return 0;
    };
    let report = format!(
        "/api/v1/game/handoff/{}",
        handed["token"].as_str().unwrap_or_default()
    );
    let mut child = match game(command, &handed, launch).spawn() {
        Ok(child) => child,
        Err(e) => {
            eprintln!("The game didn't start — {e}");
            let _ = call(client, &report, &json!({ "event": "exited", "code": 127 }));
            return 127;
        }
    };
    pass_term_to(child.id());
    let _ = call(
        client,
        &report,
        &json!({ "event": "started", "pid": child.id() }),
    );
    let code = match child.wait() {
        Ok(status) => {
            use std::os::unix::process::ExitStatusExt;
            status
                .code()
                .unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
        }
        Err(_) => 1,
    };
    let _ = call(client, &report, &json!({ "event": "exited", "code": code }));
    wait_until_gone(client, app);
    code
}

/// `sh -c command` in a process group of its own, with this process's environment, Steam's
/// library path and preload back, and the handoff's `env` on top.
fn game(command: &str, handed: &Value, launch: &Launch) -> std::process::Command {
    use std::os::unix::process::CommandExt;
    let mut c = std::process::Command::new("sh");
    c.arg("-c").arg(command).process_group(0);
    for (key, kept) in [
        ("LD_LIBRARY_PATH", &launch.ld_library_path),
        ("LD_PRELOAD", &launch.ld_preload),
    ] {
        match kept {
            Some(value) => c.env(key, value),
            None => c.env_remove(key),
        };
    }
    c.env_remove("PF_STEAM_LD_LIBRARY_PATH")
        .env_remove("PF_STEAM_LD_PRELOAD");
    for (key, value) in handed["env"].as_object().into_iter().flatten() {
        if let Some(value) = value.as_str() {
            c.env(key, value);
        }
    }
    if let Some(dir) = handed["cwd"].as_str() {
        c.current_dir(dir);
    }
    c
}

/// POSTs `body` to `path`: the answer's JSON, or the exit code once the failure is on stderr.
fn call(client: &Client, path: &str, body: &Value) -> Result<Value, i32> {
    let body = body.to_string().into_bytes();
    let json = Some("application/json");
    let (status, _, reader) = client
        .send("POST", path, json, None, None, body)
        .map_err(|f| failed(&f))?;
    let bytes = read_capped(reader, 1 << 20).unwrap_or_default();
    let answer: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    if (200..300).contains(&status) {
        return Ok(answer);
    }
    let said = answer["error"]
        .as_str()
        .unwrap_or("The host refused the game.");
    eprintln!("{said}");
    Err(1)
}

/// The sentence for a host this process can't reach, and the exit code.
fn failed(f: &Failure) -> i32 {
    if f.code == EXIT_UNREACHABLE {
        eprintln!("Punktfunk isn't running on this device. Start it, then start the game again.");
    } else {
        eprintln!("{}", f.message);
    }
    f.code
}

/// Waits until the host no longer lists title `app` running at its own screen, or no longer
/// answers: a launcher that handed off and exited must not end Steam's entry while its game
/// runs. A row that says why it ended is over.
fn wait_until_gone(client: &Client, app: &str) {
    while let Ok(status) = client.get("/api/v1/status") {
        let mut games = status["games"].as_array().into_iter().flatten();
        if !games.any(|g| g["app_id"] == app && g["plane"] == "local" && g["state"] != "exited") {
            return;
        }
        std::thread::sleep(POLL);
    }
}

/// The game's process group, which a SIGTERM to this process is passed to. 0 before the game.
static GROUP: AtomicI32 = AtomicI32::new(0);

extern "C" fn pass_on(signal: libc::c_int) {
    let group = GROUP.load(Ordering::Relaxed);
    if group > 0 {
        // SAFETY: `kill` is async-signal-safe and touches no memory of ours; `-group` is the
        // process group this process made for the game.
        unsafe { libc::kill(-group, signal) };
    }
}

/// Steam's Exit sends this process SIGTERM: from now on it reaches the group game `pid` leads.
fn pass_term_to(pid: u32) {
    GROUP.store(pid as i32, Ordering::Relaxed);
    let handler = pass_on as extern "C" fn(libc::c_int);
    // SAFETY: the handler only loads an atomic and calls `kill`, both async-signal-safe, and it
    // lives for the whole process.
    unsafe { libc::signal(libc::SIGTERM, handler as libc::sighandler_t) };
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    /// What a fake host took: method, path and body of each request, in order.
    type Seen = Arc<Mutex<Vec<(String, String, Value)>>>;

    /// What a fake host answers a request, given those it took before: a status and a body.
    type Answer = Box<dyn Fn(&str, &str, &[(String, String, Value)]) -> (u16, String) + Send>;

    /// Writes `dir` as `ctl` finds a host, with a port nothing listens on yet.
    fn host_files(dir: &Path, cert_pem: &str, port: u16) {
        std::fs::write(dir.join("native-cert.pem"), cert_pem).unwrap();
        std::fs::write(dir.join("mgmt-token"), "PUNKTFUNK_MGMT_TOKEN=play-test\n").unwrap();
        let url = format!("PUNKTFUNK_MGMT_URL=https://127.0.0.1:{port}\n");
        std::fs::write(dir.join("mgmt-endpoint"), url).unwrap();
    }

    /// A host on loopback with a certificate of its own, written into `dir`, that answers each
    /// request with `answer` over one TLS connection apiece.
    fn fake_host(dir: &Path, answer: Answer) -> Seen {
        punktfunk_core::tls::install_default_provider();
        let id = crate::identity::ephemeral().unwrap();
        let tls = crate::https::server_config_optional_client(&id.cert_pem, &id.key_pem).unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        host_files(dir, &id.cert_pem, listener.local_addr().unwrap().port());
        let seen = Seen::default();
        let took = seen.clone();
        std::thread::spawn(move || {
            for sock in listener.incoming().flatten() {
                let conn = rustls::ServerConnection::new(tls.clone()).unwrap();
                let mut stream = rustls::StreamOwned::new(conn, sock);
                let Some((method, path, body)) = request(&mut stream) else {
                    continue;
                };
                let (status, reply) = answer(&method, &path, &took.lock().unwrap());
                took.lock().unwrap().push((method, path, body));
                let head = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n",
                    reply.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(reply.as_bytes());
                stream.conn.send_close_notify();
                let _ = stream.flush();
            }
        });
        seen
    }

    /// One HTTP request off `stream`: its method, path and JSON body.
    fn request(stream: &mut impl std::io::Read) -> Option<(String, String, Value)> {
        let mut raw = Vec::new();
        let mut byte = [0u8; 1];
        while !raw.ends_with(b"\r\n\r\n") {
            stream.read_exact(&mut byte).ok()?;
            raw.push(byte[0]);
        }
        let head = String::from_utf8_lossy(&raw).into_owned();
        let mut words = head.split_whitespace();
        let (method, path) = (words.next()?.to_string(), words.next()?.to_string());
        let length = head
            .lines()
            .find_map(|l| {
                l.to_ascii_lowercase()
                    .strip_prefix("content-length:")?
                    .trim()
                    .parse()
                    .ok()
            })
            .unwrap_or(0);
        let mut body = vec![0u8; length];
        stream.read_exact(&mut body).ok()?;
        Some((
            method,
            path,
            serde_json::from_slice(&body).unwrap_or(Value::Null),
        ))
    }

    fn launch() -> Launch {
        Launch {
            gameid: "12412508506844823552".into(),
            sdl_ignore: "0x28de/0x1205".into(),
            sdl_ignore_except: String::new(),
            ld_library_path: Some("/steam/lib".into()),
            ld_preload: None,
        }
    }

    const TOKEN: &str = "handoff-token";

    /// `play` asks with Steam's facts, starts the command with Steam's library path back, no
    /// preload and the recipe's env on top, reports start and exit, stays while the host lists
    /// the title, and exits with the game's code.
    #[test]
    fn play_runs_the_handed_over_game_until_the_host_lets_go() {
        let dir = tempfile::tempdir().unwrap();
        let check = r#"[ "$LD_LIBRARY_PATH" = /steam/lib ] && [ -z "${LD_PRELOAD+x}" ] && [ "$PF_PLAY_TEST" = "on top" ] && exit 3; exit 4"#;
        let handed = json!({ "token": TOKEN, "app_id": "custom:play", "title": "Play",
            "command": check, "env": { "PF_PLAY_TEST": "on top" }, "cwd": null });
        let seen = fake_host(
            dir.path(),
            Box::new(move |method, path, before| match (method, path) {
                ("POST", "/api/v1/game/handoff") => (200, handed.to_string()),
                ("GET", "/api/v1/status") => {
                    let asked = before.iter().any(|(_, p, _)| p == "/api/v1/status");
                    let row =
                        json!({ "app_id": "custom:play", "plane": "local", "state": "running" });
                    let games = if asked { json!([]) } else { json!([row]) };
                    (200, json!({ "games": games }).to_string())
                }
                _ => (204, String::new()),
            }),
        );
        let client = Client::connect_in(dir.path(), Some(Duration::from_secs(10))).unwrap();
        assert_eq!(run(&client, &launch()), 3, "the game's own exit code");

        let seen = seen.lock().unwrap();
        let paths: Vec<&str> = seen.iter().map(|(_, p, _)| p.as_str()).collect();
        let report = format!("/api/v1/game/handoff/{TOKEN}");
        let report = report.as_str();
        let want = ["/api/v1/game/handoff", report, report];
        assert_eq!(paths, [&want[..], &["/api/v1/status"; 2]].concat());
        let asked = &seen[0].2;
        assert_eq!(asked["steam_gameid"], "12412508506844823552");
        assert_eq!(asked["sdl_ignore"], "0x28de/0x1205");
        assert_eq!(asked["sdl_ignore_except"], "");
        assert_eq!(seen[1].2["event"], "started");
        assert!(seen[1].2["pid"].as_u64().is_some_and(|p| p > 1));
        assert_eq!(seen[2].2, json!({ "event": "exited", "code": 3 }));
    }

    /// A refusal is the host's sentence and a failing exit; nothing starts and nothing reports.
    #[test]
    fn a_refused_handoff_starts_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let refusal =
            json!({ "error": "This game isn't in Steam yet.", "code": "local_not_in_steam" });
        let seen = fake_host(
            dir.path(),
            Box::new(move |_, _, _| (409, refusal.to_string())),
        );
        let client = Client::connect_in(dir.path(), Some(Duration::from_secs(10))).unwrap();
        assert_eq!(run(&client, &launch()), 1);
        assert_eq!(seen.lock().unwrap().len(), 1, "the handoff alone");
    }

    /// A host that is not running is a failing exit with nothing started.
    #[test]
    fn no_running_host_is_a_failing_exit() {
        let dir = tempfile::tempdir().unwrap();
        let id = crate::identity::ephemeral().unwrap();
        let port = {
            let gone = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            gone.local_addr().unwrap().port()
        };
        host_files(dir.path(), &id.cert_pem, port);
        let client = Client::connect_in(dir.path(), Some(Duration::from_secs(10))).unwrap();
        assert_eq!(run(&client, &launch()), EXIT_UNREACHABLE);
    }
}
