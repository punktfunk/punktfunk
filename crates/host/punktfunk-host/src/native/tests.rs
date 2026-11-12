use super::*;
use punktfunk_core::quic::v2::msg::V2Message;

/// Two sessions on one profile at once: `<seat>` then `<seat>-2`, each with its own sink,
/// and the id frees when its session ends.
#[cfg(target_os = "linux")]
#[test]
fn a_second_session_on_a_profile_gets_planes_of_its_own() {
    let base = seat_id("5e1f00d1e2a7");
    assert_eq!(base, "5e1f00d1");
    let first = SeatClaim::take(&base);
    let second = SeatClaim::take(&base);
    assert_eq!(
        (first.0.as_str(), second.0.as_str()),
        ("5e1f00d1", "5e1f00d1-2")
    );
    let (a, b) = (session_isolation(&first.0), session_isolation(&second.0));
    assert_ne!(a.ei_relay, b.ei_relay);
    assert_ne!(a.mic_source, b.mic_source);
    if a.sink.is_some() {
        assert_ne!(a.sink, b.sink);
    }
    drop(first);
    let again = SeatClaim::take(&base);
    assert_eq!(again.0, "5e1f00d1", "a freed id is taken again");
}

/// The accept loop's address-validation gate. A first contact is unvalidated; a Retry turns
/// it into a second, validated arrival, and the client completes anyway. Pins the quinn
/// behaviour the gate rests on — a release that validated first contact would leave the
/// branch dead, and one that refused a legal retry would drop every new client.
#[test]
fn an_unvalidated_first_contact_is_retried_then_accepted() {
    use punktfunk_core::quic::endpoint;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let server = endpoint::server("127.0.0.1:0".parse().unwrap()).unwrap();
        let addr = server.local_addr().unwrap();
        let accept = tokio::spawn(async move {
            let mut arrivals = Vec::new();
            while let Some(incoming) = server.accept().await {
                let validated = incoming.remote_address_validated();
                arrivals.push(validated);
                // The same gate `serve` applies before it spends a task on a source.
                if !validated {
                    assert!(
                        incoming.retry().is_ok(),
                        "retry is legal whenever the source is unvalidated"
                    );
                    continue;
                }
                let conn = incoming.await.expect("host side of the retried handshake");
                // Hold the endpoint: dropping it would close the connection under the client.
                return (arrivals, server, conn);
            }
            panic!("endpoint closed before a validated arrival");
        });
        let client = endpoint::client_insecure().unwrap();
        let client_conn = client
            .connect(addr, "punktfunk")
            .unwrap()
            .await
            .expect("client completes across the Retry");
        let (arrivals, _server, _host_conn) = accept.await.unwrap();
        // An Initial the client retransmits before the Retry lands arrives unvalidated too,
        // so pin the shape rather than the count: every arrival we turned away was
        // unvalidated, and the one we accepted had proved its address.
        let (accepted, retried) = arrivals.split_last().expect("at least one arrival");
        assert!(
            accepted,
            "the accepted arrival is address-validated: {arrivals:?}"
        );
        assert!(
            !retried.is_empty() && retried.iter().all(|v| !v),
            "first contact is unvalidated and gets retried: {arrivals:?}"
        );
        drop(client_conn);
    });
}

/// The pipeline raises a pin miss under two layers of `.context`, so the close
/// carries the user's sentence only if the downcast walks the whole chain.
/// Reads the error `resolve` really produces, not a stand-in.
#[test]
fn a_buried_pin_miss_still_reaches_the_client_in_words() {
    use anyhow::Context;
    let heads = [pf_vdisplay::monitors::PhysicalMonitor {
        connector: "HDMI-A-1".into(),
        description: String::new(),
        width: 1920,
        height: 1080,
        refresh_mhz: 60000,
        x: 0,
        y: 0,
        scale: 1.0,
        primary: true,
        enabled: true,
        managed: false,
    }];
    let buried = pf_vdisplay::monitors::resolve(&heads, "HDMI-A-3")
        .map(|_| ())
        .context("create virtual output")
        .context("build the session pipeline")
        .unwrap_err();

    let worded = setup_failed_sentence(&buried).expect("a pin miss has words for the user");
    assert_eq!(worded.id, "close_monitor_missing");
    assert_eq!(worded.params, [("monitor".into(), "HDMI-A-3".into())]);
    let said = worded.text;
    assert!(
        said.contains("HDMI-A-3") && said.contains("HDMI-A-1"),
        "{said}"
    );
    assert!(
        said.len() <= punktfunk_core::quic::REFUSED_REASON_MAX,
        "one close frame, uncut: {} bytes",
        said.len()
    );

    let asleep = anyhow::Error::new(pf_vdisplay::DisplayAsleep)
        .context("acquire virtual output for the session (retry-hold lease)");
    let said = setup_failed_sentence(&asleep)
        .expect("a dark display has words for the user")
        .text;
    assert!(said.starts_with("The host's screen is asleep"), "{said}");
    assert!(said.len() <= punktfunk_core::quic::REFUSED_REASON_MAX);

    // Anything else keeps the client's own wording rather than leaking a chain.
    let other = anyhow::anyhow!("open NVENC: device busy").context("build the session pipeline");
    assert_eq!(setup_failed_sentence(&other), None);
}

/// Stock niri names biri to the player, whatever `.context` the pipeline wrapped it in.
#[cfg(target_os = "linux")]
#[test]
fn stock_niri_reaches_the_client_in_words() {
    use anyhow::Context;
    let buried = Err::<(), _>(pf_vdisplay::NoVirtualOutputs)
        .context("create virtual output")
        .context("build the session pipeline")
        .unwrap_err();

    let said = setup_failed_sentence(&buried)
        .expect("stock niri has words for the user")
        .text;
    assert!(said.contains("biri"), "{said}");
    assert!(said.len() <= punktfunk_core::quic::REFUSED_REASON_MAX);
}

/// An Umbriel whose IPC this host can't parse tells the player what to update.
#[cfg(target_os = "linux")]
#[test]
fn an_umbriel_ipc_mismatch_reaches_the_client_in_words() {
    let buried = anyhow::Error::new(pf_vdisplay::UmbrielIpcMismatch)
        .context("create virtual output")
        .context("build the session pipeline");
    let said = setup_failed_sentence(&buried)
        .expect("a mismatch has words for the user")
        .text;
    assert!(said.contains("Update Punktfunk"), "{said}");
    assert!(said.len() <= punktfunk_core::quic::REFUSED_REASON_MAX);
}

/// A joiner whose own stream beside the shared one got no frame is told what to change.
#[cfg(target_os = "linux")]
#[test]
fn a_shared_capture_in_another_format_reaches_the_client_in_words() {
    let buried = anyhow::anyhow!("no PipeWire frame within 10s (node 66)")
        .context(pf_capture::SharedCaptureTaken)
        .context("first frame")
        .context("build the session pipeline");
    let said = setup_failed_sentence(&buried)
        .expect("a taken shared capture has words for the user")
        .text;
    assert!(said.contains("same video codec"), "{said}");
    assert!(said.len() <= punktfunk_core::quic::REFUSED_REASON_MAX);
}

/// A node-less output that direct capture refused reaches the client in words.
#[cfg(target_os = "linux")]
#[test]
fn a_node_less_output_reaches_the_client_in_words() {
    let buried = anyhow::anyhow!("no ext_image_copy_capture_manager_v1")
        .context(pf_capture::NoPipeWireNode)
        .context("open the session capture")
        .context("build the session pipeline");
    let said = setup_failed_sentence(&buried)
        .expect("a node-less output has words for the user")
        .text;
    assert!(said.contains("GPU encoder"), "{said}");
    assert!(said.len() <= punktfunk_core::quic::REFUSED_REASON_MAX);
}

#[test]
fn live_mode_pack_roundtrips_and_interval_recovers_hz() {
    use crate::session_status::{pack_mode, unpack_mode};
    // Pack → unpack is exact for real modes.
    for (w, h, hz) in [(1280u32, 720u32, 60u32), (3840, 2160, 144), (320, 200, 24)] {
        assert_eq!(unpack_mode(pack_mode(w, h, hz)), (w, h, hz));
    }
    // `interval` is 1/effective_hz — the round-trip recovers the integer rate.
    for hz in [24u32, 30, 60, 75, 90, 120, 144, 165, 240] {
        let interval = std::time::Duration::from_secs_f64(1.0 / hz as f64);
        assert_eq!(interval_hz(interval), hz);
    }
}

#[test]
fn delivered_mode_reports_captured_dims_and_triggers_corrective_ack() {
    let hz60 = std::time::Duration::from_secs_f64(1.0 / 60.0);
    let requested = punktfunk_core::Mode {
        width: 2560,
        height: 1440,
        refresh_hz: 60,
    };

    // Honored: captured frame matches the request → no corrective ack.
    let honored = delivered_mode(2560, 1440, hz60);
    assert_eq!(honored, requested);

    // Fallback dims differ from the acked request → a corrective ack is owed.
    let fell_back = delivered_mode(1920, 1080, hz60);
    assert_ne!(fell_back, requested);
    assert_eq!(
        fell_back,
        punktfunk_core::Mode {
            width: 1920,
            height: 1080,
            refresh_hz: 60
        }
    );

    // Refresh cap: same dims, achieved rate recovered from the interval.
    let capped = delivered_mode(2560, 1440, std::time::Duration::from_secs_f64(1.0 / 30.0));
    assert_ne!(capped, requested);
    assert_eq!(capped.refresh_hz, 30);
}

#[test]
fn gamepad_wire_bits_are_pinned() {
    use punktfunk_core::input::gamepad as pf;
    // buttonFlags — low 16 bits, named from core.
    assert_eq!(pf::BTN_DPAD_UP, 0x0000_0001);
    assert_eq!(pf::BTN_DPAD_DOWN, 0x0000_0002);
    assert_eq!(pf::BTN_DPAD_LEFT, 0x0000_0004);
    assert_eq!(pf::BTN_DPAD_RIGHT, 0x0000_0008);
    assert_eq!(pf::BTN_START, 0x0000_0010);
    assert_eq!(pf::BTN_BACK, 0x0000_0020);
    assert_eq!(pf::BTN_LS_CLICK, 0x0000_0040);
    assert_eq!(pf::BTN_RS_CLICK, 0x0000_0080);
    assert_eq!(pf::BTN_LB, 0x0000_0100);
    assert_eq!(pf::BTN_RB, 0x0000_0200);
    assert_eq!(pf::BTN_GUIDE, 0x0000_0400);
    assert_eq!(pf::BTN_A, 0x0000_1000);
    assert_eq!(pf::BTN_B, 0x0000_2000);
    assert_eq!(pf::BTN_X, 0x0000_4000);
    assert_eq!(pf::BTN_Y, 0x0000_8000);
    // buttonFlags2 — paddles + DualSense/DS4 touchpad-click / Share.
    assert_eq!(pf::BTN_PADDLE1, 0x0001_0000);
    assert_eq!(pf::BTN_PADDLE2, 0x0002_0000);
    assert_eq!(pf::BTN_PADDLE3, 0x0004_0000);
    assert_eq!(pf::BTN_PADDLE4, 0x0008_0000);
    assert_eq!(pf::BTN_TOUCHPAD, 0x0010_0000);
    assert_eq!(pf::BTN_MISC1, 0x0020_0000);
    // Axis ids — dense, 0-based.
    assert_eq!(
        [
            pf::AXIS_LS_X,
            pf::AXIS_LS_Y,
            pf::AXIS_RS_X,
            pf::AXIS_RS_Y,
            pf::AXIS_LT,
            pf::AXIS_RT,
        ],
        [0, 1, 2, 3, 4, 5]
    );
}

/// Pull and byte-verify `count` synthetic frames through the C ABI connection.
unsafe fn pull_verified(conn: *mut punktfunk_ffi::PunktfunkConnection, count: u32) {
    use punktfunk_core::error::PunktfunkStatus;
    let mut got = 0u32;
    // SAFETY: `PunktfunkFrame` is `#[repr(C)]` POD; all-zero is valid (null `data`, `len == 0`).
    // Read only after `next_au` overwrites it on `Ok`.
    let mut frame = unsafe { std::mem::zeroed() };
    // A host that stops sending fails the test instead of holding it to the job's timeout.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while got < count {
        assert!(
            std::time::Instant::now() < deadline,
            "{got} of {count} frames before the deadline"
        );
        // SAFETY: `conn` is the live handle from `punktfunk_connect` (caller asserts non-null,
        // does not close until after return). `&mut frame` outlives this call. This thread is
        // the only video puller.
        match unsafe { punktfunk_ffi::punktfunk_connection_next_au(conn, &mut frame, 2000) } {
            PunktfunkStatus::Ok => {
                // SAFETY: on `Ok`, `frame.data`/`len` is the connection-owned AU, valid until the
                // next `next_au` on this handle. We read the whole slice before that next call.
                let data = unsafe { std::slice::from_raw_parts(frame.data, frame.len) };
                let idx = u32::from_le_bytes(data[0..4].try_into().unwrap());
                assert_eq!(
                    data,
                    &test_frame(idx, data.len())[..],
                    "frame {idx} content"
                );
                got += 1;
            }
            PunktfunkStatus::NoFrame => continue,
            other => panic!("next_au: {other:?} after {got} of {count} frames"),
        }
    }
}

/// In-process hosts share the process-global admission table. Concurrent tests would
/// `preempt_same_identity` each other. Poison-tolerant so a failing test does not cascade.
///
/// A session here also lands in the live registry, so every holder takes
/// [`crate::session_status::tests::REGISTRY`] first — that order, always.
static SESSION_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// C ABI: TOFU connect → pull frames → send input → close. Three sequential sessions
/// against one host prove the persistent listener; a wrong pin is rejected.
#[test]
fn c_abi_connection_roundtrip() {
    let _registry = crate::session_status::tests::registry_lock();
    let _serial = SESSION_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    use punktfunk_core::error::PunktfunkStatus;
    use punktfunk_ffi::{
        punktfunk_connect, punktfunk_connection_close, punktfunk_connection_mode,
        punktfunk_connection_send_input,
    };

    let host = std::thread::spawn(|| {
        run_ephemeral(Punktfunk1Options {
            port: 19777,
            source: Punktfunk1Source::Synthetic,
            seconds: 0,
            // More than the 25 each session pulls. The budget is one loop per session and a
            // mid-stream mode change discards what is already queued at the old mode, so a
            // client that pulls the whole budget only succeeds when its switch beats frame 0.
            frames: 40,
            max_sessions: 3,
            max_concurrent: 1,
            require_pairing: false,
            allow_pairing: false,
            pairing_pin: None,
            paired_store: None,
            idle_timeout: None,
            mdns: false, // tests must not advertise on the LAN
        })
    });
    std::thread::sleep(std::time::Duration::from_millis(500));

    // Session 1: TOFU (no pin) — observe the host fingerprint.
    let addr = std::ffi::CString::new("127.0.0.1").unwrap();
    let mut observed = [0u8; 32];
    // SAFETY: `addr` is a live NUL-terminated host string; pin/cert/key are NULL (permitted);
    // `observed` is 32 writable bytes. All locals outlive the blocking connect.
    let conn = unsafe {
        punktfunk_connect(
            addr.as_ptr(),
            19777,
            1280,
            720,
            60,
            std::ptr::null(),
            observed.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            10_000,
        )
    };
    assert!(!conn.is_null(), "punktfunk_connect failed");
    assert_ne!(observed, [0u8; 32], "fingerprint not reported");

    let (mut w, mut h, mut hz) = (0u32, 0u32, 0u32);
    // SAFETY: `conn` is the live handle; `&mut w/h/hz` outlive this call.
    let st = unsafe { punktfunk_connection_mode(conn, &mut w, &mut h, &mut hz) };
    assert_eq!(st, PunktfunkStatus::Ok);
    assert_eq!((w, h, hz), (1280, 720, 60));

    // Mid-stream renegotiation: request a new mode; `punktfunk_connection_mode` reflects it.
    // SAFETY: `conn` is the live handle; remaining args are by-value. Handle outlives enqueue.
    let st = unsafe { punktfunk_ffi::punktfunk_connection_request_mode(conn, 1920, 1080, 144) };
    assert_eq!(st, PunktfunkStatus::Ok);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        // SAFETY: same as the earlier `punktfunk_connection_mode` call.
        let st = unsafe { punktfunk_connection_mode(conn, &mut w, &mut h, &mut hz) };
        assert_eq!(st, PunktfunkStatus::Ok);
        if (w, h, hz) == (1920, 1080, 144) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "mode switch not acked (still {w}x{h}@{hz})"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }

    // SAFETY: `conn` is the open handle; this thread is the only video puller.
    unsafe { pull_verified(conn, 25) };

    let ev = punktfunk_core::input::InputEvent {
        kind: punktfunk_core::input::InputKind::MouseMove,
        _pad: [0; 3],
        code: 0,
        x: 1,
        y: 2,
        flags: 0,
    };
    // SAFETY: `conn` is live; `&ev` is a valid `InputEvent` for this enqueue.
    let st = unsafe { punktfunk_connection_send_input(conn, &ev) };
    assert_eq!(st, PunktfunkStatus::Ok);
    // SAFETY: `conn` is unused after this; `close` frees it once. Session 2 uses `conn2`.
    unsafe { punktfunk_connection_close(conn) };

    // Session 2 (same host process): pin the fingerprint.
    // SAFETY: as session 1 — `observed.as_ptr()` is the 32-byte pin; out/cert/key are NULL.
    let conn2 = unsafe {
        punktfunk_connect(
            addr.as_ptr(),
            19777,
            1280,
            720,
            60,
            observed.as_ptr(),
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
            10_000,
        )
    };
    assert!(!conn2.is_null(), "pinned reconnect failed");
    // SAFETY: `conn2` is the live pinned handle; this thread is the only puller.
    unsafe { pull_verified(conn2, 25) };
    // SAFETY: `conn2` is unused after this; `close` frees it once.
    unsafe { punktfunk_connection_close(conn2) };

    // Session 3: a wrong pin must be rejected.
    let bad = [0xAAu8; 32];
    // SAFETY: `bad.as_ptr()` is the 32-byte pin; out/cert/key are NULL. Expected to return NULL.
    let conn3 = unsafe {
        punktfunk_connect(
            addr.as_ptr(),
            19777,
            1280,
            720,
            60,
            bad.as_ptr(),
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
            10_000,
        )
    };
    assert!(conn3.is_null(), "wrong pin must fail the handshake");

    // TLS-failed handshake never yields a connection, so accept() is still waiting.
    // One more TOFU connect completes the host's third session.
    // SAFETY: same as session 1 — pin/out/cert/key all NULL.
    let conn4 = unsafe {
        punktfunk_connect(
            addr.as_ptr(),
            19777,
            1280,
            720,
            60,
            std::ptr::null(),
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
            10_000,
        )
    };
    assert!(!conn4.is_null());
    // SAFETY: `conn4` is live; this thread is the only puller.
    unsafe { pull_verified(conn4, 25) };
    // SAFETY: `conn4` is unused after this; `close` frees it once.
    unsafe { punktfunk_connection_close(conn4) };

    host.join().unwrap().unwrap();
}

/// A `synthetic-abr` session publishes a registry row while it streams and retires it
/// when it ends. The row's id is the one the control task reads off the session's
/// counters before it asks the governor for a share, so a source that never registers
/// leaves a shared path undivided. The row names the preset this dial carried.
#[test]
fn a_synthetic_abr_session_registers_while_it_streams() {
    let _registry = crate::session_status::tests::registry_lock();
    let _serial = SESSION_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    use punktfunk_core::client::{ConnectParams, NativeClient};

    let host = std::thread::spawn(|| {
        run_ephemeral(Punktfunk1Options {
            port: 19782,
            source: Punktfunk1Source::SyntheticAbr(SynthAbrShape {
                content: Content::Steady { fill_pct: 100 },
                recovery: std::time::Duration::ZERO,
                answer: KeyframeAnswer::Idr,
                idr_pct: DEFAULT_IDR_PCT,
                bringup: std::time::Duration::ZERO,
                serve_ramp: false,
            }),
            seconds: 3,
            frames: 0, // this source is timed, not counted
            max_sessions: 1,
            max_concurrent: 1,
            require_pairing: false,
            allow_pairing: false,
            pairing_pin: None,
            paired_store: None,
            idle_timeout: None,
            mdns: false,
        })
    });
    std::thread::sleep(std::time::Duration::from_millis(500));

    let mode = punktfunk_core::Mode {
        width: 1280,
        height: 720,
        refresh_hz: 60,
    };
    let client = NativeClient::connect(ConnectParams {
        preset: punktfunk_core::quic::SessionPreset::new("dock-1", "Docked"),
        ..ConnectParams::new("127.0.0.1", 19782, mode, std::time::Duration::from_secs(10))
    })
    .expect("client connects to the synthetic-abr host");

    // The registry is process-global and the session_status tests register their own
    // rows in it; this mode is what tells ours apart from theirs.
    let ours = || {
        crate::session_status::snapshot()
            .into_iter()
            .find(|s| (s.width, s.height, s.fps) == (1280, 720, 60))
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let row = loop {
        if let Some(r) = ours() {
            break r;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the synthetic-abr session never reached the registry"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    assert_ne!(
        row.id, 0,
        "0 is the id the control task skips the governor on"
    );
    assert_eq!(row.plane, crate::events::Plane::Native);
    assert_eq!(row.preset_name.as_deref(), Some("Docked"));

    drop(client);
    host.join().unwrap().unwrap();
    assert!(
        ours().is_none(),
        "the guard retires the row on the stream's exit path"
    );
}

/// Set on the clipboard test's child only.
const CLIPBOARD_CHILD: &str = "PF_CLIPBOARD_TEST_CHILD";

/// Clipboard over a synthetic session: host advertises the cap, acks enable with
/// `BACKEND_UNAVAILABLE` (no compositor), declines a fetch. Live-backend paths are
/// not covered here. `design/clipboard-and-file-transfer.md`. The operator policy is process
/// config, so the body runs in a child of this test binary started with it on: no thread of
/// this process writes the environment while others read it.
#[test]
fn clipboard_control_and_fetch_decline_over_session() {
    if std::env::var_os(CLIPBOARD_CHILD).is_none() {
        let module = module_path!().split_once("::").expect("crate::module").1;
        let test = format!("{module}::clipboard_control_and_fetch_decline_over_session");
        let mut child =
            std::process::Command::new(std::env::current_exe().expect("this test binary"));
        child.args(["--exact", &test, "--nocapture"]);
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("PUNKTFUNK_") {
                child.env_remove(key);
            }
        }
        let out = child
            .env(CLIPBOARD_CHILD, "1")
            .env("PUNKTFUNK_CLIPBOARD", "1")
            .output()
            .expect("run the child");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success(),
            "{stdout}{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            stdout.contains("1 passed"),
            "the child ran no test: {stdout}"
        );
        return;
    }
    let _registry = crate::session_status::tests::registry_lock();
    let _serial = SESSION_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    use punktfunk_core::client::{ConnectParams, NativeClient};
    use punktfunk_core::clipboard::ClipEventCore;
    use punktfunk_core::quic::{
        CLIP_FILE_INDEX_NONE, CLIP_FLAG_FILES, CLIP_POLICY_FILES, HOST_CAP_CLIPBOARD,
    };

    let host = std::thread::spawn(|| {
        run_ephemeral(Punktfunk1Options {
            port: 19781,
            source: Punktfunk1Source::Synthetic,
            seconds: 0,
            frames: 600, // outlive the control exchange
            max_sessions: 1,
            max_concurrent: 1,
            require_pairing: false,
            allow_pairing: false,
            pairing_pin: None,
            paired_store: None,
            idle_timeout: None,
            mdns: false,
        })
    });
    std::thread::sleep(std::time::Duration::from_millis(500));

    let mode = punktfunk_core::Mode {
        width: 1280,
        height: 720,
        refresh_hz: 60,
    };
    let client = NativeClient::connect(ConnectParams::new(
        "127.0.0.1",
        19781,
        mode,
        std::time::Duration::from_secs(10),
    ))
    .expect("client connects to synthetic host");

    assert_ne!(
        client.host_caps() & HOST_CAP_CLIPBOARD,
        0,
        "an enabled host advertises HOST_CAP_CLIPBOARD"
    );

    // Bounded poll over the clipboard event plane.
    let poll = |pred: &dyn Fn(&ClipEventCore) -> bool| -> Option<ClipEventCore> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            match client.next_clip(std::time::Duration::from_millis(200)) {
                Ok(ev) if pred(&ev) => return Some(ev),
                Ok(_) => {}
                Err(punktfunk_core::PunktfunkError::NoFrame) => {}
                Err(_) => break,
            }
        }
        None
    };

    // Enable (files): synthetic has no backend → BACKEND_UNAVAILABLE, policy still reports files.
    client.clip_control(true, CLIP_FLAG_FILES).unwrap();
    let state = poll(&|e| matches!(e, ClipEventCore::State { .. }))
        .expect("host replies with a ClipState ack");
    match state {
        ClipEventCore::State {
            enabled,
            policy,
            reason,
        } => {
            assert!(!enabled, "no backend for a synthetic session → not enabled");
            assert_eq!(
                reason,
                punktfunk_core::quic::CLIP_REASON_BACKEND_UNAVAILABLE,
                "the refusal reason is BACKEND_UNAVAILABLE"
            );
            assert_ne!(
                policy & CLIP_POLICY_FILES,
                0,
                "PUNKTFUNK_CLIPBOARD=1 permits files"
            );
        }
        _ => unreachable!(),
    }

    // Fetch: no backend → Error for that transfer id.
    let xfer = client
        .clip_fetch(1, "text/plain;charset=utf-8".into(), CLIP_FILE_INDEX_NONE)
        .unwrap();
    let err = poll(&|e| matches!(e, ClipEventCore::Error { id, .. } if *id == xfer))
        .expect("host declines the fetch (no backend) → Error event");
    assert!(matches!(err, ClipEventCore::Error { .. }));

    drop(client);
    host.join().unwrap().unwrap();
}

/// Spin up a host of `source` on `port` and dial it with `params`; the host joins on
/// drop of the returned client.
fn synthetic_session(
    port: u16,
    source: Punktfunk1Source,
    params: impl FnOnce(punktfunk_core::client::ConnectParams) -> punktfunk_core::client::ConnectParams,
) -> (
    punktfunk_core::client::NativeClient,
    std::thread::JoinHandle<anyhow::Result<()>>,
) {
    use punktfunk_core::client::{ConnectParams, NativeClient};
    let host = std::thread::spawn(move || {
        run_ephemeral(Punktfunk1Options {
            port,
            source,
            seconds: 0,
            frames: 600,
            max_sessions: 1,
            max_concurrent: 1,
            require_pairing: false,
            allow_pairing: false,
            pairing_pin: None,
            paired_store: None,
            idle_timeout: None,
            mdns: false,
        })
    });
    std::thread::sleep(std::time::Duration::from_millis(500));
    let mode = punktfunk_core::Mode {
        width: 1280,
        height: 720,
        refresh_hz: 60,
    };
    let client = NativeClient::connect(params(ConnectParams::new(
        "127.0.0.1",
        port,
        mode,
        std::time::Duration::from_secs(10),
    )))
    .expect("client connects to synthetic host");
    (client, host)
}

fn wait_for(pred: impl Fn() -> bool) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if pred() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    false
}

/// The first `StreamConfig` carries the host's end of the path: the data socket's send
/// buffer at least, and the interface's kind and speed where the OS says.
#[test]
fn the_stream_config_carries_the_hosts_link_facts() {
    let _registry = crate::session_status::tests::registry_lock();
    let _serial = SESSION_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let (client, host) = synthetic_session(19783, Punktfunk1Source::Synthetic, |p| p);
    assert!(
        wait_for(|| client.host_link().sndbuf_kb > 0),
        "the data socket has a send buffer"
    );
    drop(client);
    host.join().unwrap().unwrap();
}

/// A client that rebuilds its decoder asks off its codec; the host answers with a new epoch and
/// the first frame of it carries the new codec. With nothing left to move to, the ask is refused
/// and the stream stays.
#[test]
fn a_codec_ask_moves_the_stream_to_another_codec_on_a_new_epoch() {
    use punktfunk_core::quic::{CODEC_H264, CODEC_HEVC};
    let _registry = crate::session_status::tests::registry_lock();
    let _serial = SESSION_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let (client, host) = synthetic_session(19789, Punktfunk1Source::Synthetic, |p| {
        punktfunk_core::client::ConnectParams {
            video_codecs: CODEC_H264 | CODEC_HEVC,
            rebuilds_decoder: true,
            ..p
        }
    });
    let first = client.video_format().codec;
    client
        .request_codec_change(first)
        .expect("format epochs are in force");
    let moved = (0..240)
        .find_map(|_| {
            client
                .next_frame(std::time::Duration::from_secs(2))
                .ok()
                .and_then(|f| f.new_format)
        })
        .expect("a frame in the new format");
    assert_ne!(moved.codec, first);
    assert_eq!(client.video_format(), moved);

    client.request_codec_change(moved.codec).unwrap();
    assert!(
        wait_for(|| client.codec_refused() == moved.codec),
        "no codec is left to move to"
    );
    assert_eq!(client.video_format(), moved);
    drop(client);
    host.join().unwrap().unwrap();
}

/// A mode switch the host refuses moves the client's refusal count and leaves the mode.
#[test]
fn a_refused_mode_switch_is_counted() {
    let _registry = crate::session_status::tests::registry_lock();
    let _serial = SESSION_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let (client, host) = synthetic_session(19792, Punktfunk1Source::Synthetic, |p| p);
    let before = client.mode();
    let odd = punktfunk_core::Mode {
        width: 1281,
        height: 721,
        refresh_hz: 60,
    };
    client.request_mode(odd).unwrap();
    assert!(
        wait_for(|| client.mode_refusals() == 1),
        "an odd size is refused"
    );
    assert_eq!(client.mode(), before);
    drop(client);
    host.join().unwrap().unwrap();
}

/// A keyframe ask leaves as a feedback datagram and the host answers it: the synthetic
/// source's IDR is ten ordinary frames.
#[test]
fn a_keyframe_ask_rides_the_feedback_datagram() {
    let _registry = crate::session_status::tests::registry_lock();
    let _serial = SESSION_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let source = Punktfunk1Source::SyntheticAbr(SynthAbrShape {
        content: Content::Steady { fill_pct: 100 },
        recovery: std::time::Duration::ZERO,
        answer: KeyframeAnswer::Idr,
        idr_pct: DEFAULT_IDR_PCT,
        bringup: std::time::Duration::ZERO,
        serve_ramp: false,
    });
    let (client, host) =
        synthetic_session(19784, source, |p| punktfunk_core::client::ConnectParams {
            bitrate_kbps: 10_000,
            ..p
        });
    let next_len = || {
        client
            .next_frame(std::time::Duration::from_secs(2))
            .map(|f| f.data.len())
    };
    let mut sizes: Vec<usize> = (0..40).filter_map(|_| next_len().ok()).collect();
    sizes.sort_unstable();
    let ordinary = sizes[sizes.len() / 2];
    client.request_keyframe().unwrap();
    let answered = (0..120).any(|_| next_len().is_ok_and(|n| n > ordinary * 5));
    assert!(
        answered,
        "no IDR followed the ask (ordinary frame {ordinary} B)"
    );
    drop(client);
    host.join().unwrap().unwrap();
}

/// A device that dials again while its first session still streams gets in once that
/// session has released, not after a fixed grace: the old 1.5 s sleep is gone.
#[test]
fn a_reconnect_waits_for_the_release_not_a_timer() {
    let _registry = crate::session_status::tests::registry_lock();
    let _serial = SESSION_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    use punktfunk_core::client::{ConnectParams, NativeClient};
    let host = std::thread::spawn(|| {
        run_ephemeral(Punktfunk1Options {
            port: 19793,
            source: Punktfunk1Source::Synthetic,
            seconds: 0,
            frames: 600,
            max_sessions: 2,
            max_concurrent: 2,
            require_pairing: false,
            allow_pairing: false,
            pairing_pin: None,
            paired_store: None,
            idle_timeout: None,
            mdns: false,
        })
    });
    std::thread::sleep(std::time::Duration::from_millis(500));
    let (cert, key) = punktfunk_core::quic::endpoint::generate_identity().unwrap();
    let dial = || {
        NativeClient::connect(ConnectParams {
            identity: Some((cert.clone(), key.clone())),
            ..ConnectParams::new(
                "127.0.0.1",
                19793,
                punktfunk_core::Mode {
                    width: 1280,
                    height: 720,
                    refresh_hz: 60,
                },
                std::time::Duration::from_secs(10),
            )
        })
        .expect("client connects")
    };
    let first = dial();
    assert!(first.next_frame(std::time::Duration::from_secs(5)).is_ok());
    let started = std::time::Instant::now();
    let second = dial();
    let took = started.elapsed();
    assert!(
        took < std::time::Duration::from_millis(1400),
        "the reconnect took {took:?}"
    );
    assert!(second.next_frame(std::time::Duration::from_secs(5)).is_ok());
    assert!(
        wait_for(|| first.end_reason() != punktfunk_core::client::PunktfunkEndReason::None),
        "the first session was retired, not kept beside the second"
    );
    drop((first, second));
    host.join().unwrap().unwrap();
}

/// A client streams over `punktfunk/2`: the handshake crosses the translated control stream,
/// the media arrives on the connection's own socket under exporter keys, and every frame is
/// the host's byte for byte. Each frame's `HostTiming` names it by the
/// session-clock pts it arrived with. Control round trips keep working.
#[test]
fn a_punktfunk_2_session_streams_end_to_end() {
    let _registry = crate::session_status::tests::registry_lock();
    let _serial = SESSION_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let (client, host) = synthetic_session(19791, Punktfunk1Source::Synthetic, |p| p);
    let mut got = 0;
    let mut pts = std::collections::HashSet::new();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while got < 60 && std::time::Instant::now() < deadline {
        if let Ok(f) = client.next_frame(std::time::Duration::from_millis(200)) {
            let idx = u32::from_le_bytes(f.data[0..4].try_into().unwrap());
            assert_eq!(f.data, test_frame(idx, f.data.len()), "frame {idx}");
            pts.insert(f.pts_ns);
            got += 1;
        }
    }
    assert_eq!(got, 60, "frames cross the v2 media path");
    let mut named = 0;
    while let Ok(t) = client.next_host_timing(std::time::Duration::from_millis(50)) {
        named += usize::from(pts.contains(&t.pts_ns));
    }
    assert!(named >= 50, "HostTiming names its frames: {named} of 60");
    client.request_probe(5_000, 200).unwrap();
    assert!(
        wait_for(|| client.probe_result().done),
        "a control round trip crosses the translated stream"
    );
    drop(client);
    host.join().unwrap().unwrap();
}

/// A `Start` that asks for probes only gets a session that serves every probe in full,
/// back to back, and shows no video: nothing was built to show.
#[test]
fn a_probe_only_start_serves_probes_without_a_pipeline() {
    let _registry = crate::session_status::tests::registry_lock();
    let _serial = SESSION_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let (client, host) = synthetic_session(19787, Punktfunk1Source::Synthetic, |p| {
        punktfunk_core::client::ConnectParams {
            probe_only: true,
            ..p
        }
    });
    assert!(client.probe_only());
    // Two long rounds back to back: a streaming session would clamp neither and
    // refuse the second for ten seconds.
    for _ in 0..2 {
        client.request_probe(20_000, 600).unwrap();
        assert!(wait_for(|| client.probe_result().done), "the round reports");
        let r = client.probe_result();
        assert!(r.wire_packets_sent > 0, "served, not declined");
        assert!(
            r.elapsed_ms >= 300,
            "served in full, not as a 50 ms step: {}",
            r.elapsed_ms
        );
    }
    assert!(matches!(
        client.next_frame(std::time::Duration::from_millis(500)),
        Err(punktfunk_core::PunktfunkError::NoFrame)
    ));
    drop(client);
    host.join().unwrap().unwrap();
}

/// The whole check over a probe-only session: the ramp proves a ceiling, the clean
/// round runs under it, both shaped legs run back to back, and the host's facts arrive.
#[test]
fn the_network_check_runs_its_legs_over_a_probe_only_session() {
    let _registry = crate::session_status::tests::registry_lock();
    let _serial = SESSION_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    use punktfunk_core::client::health::{self, LegShape};
    let source = Punktfunk1Source::SyntheticAbr(SynthAbrShape {
        content: Content::Steady { fill_pct: 100 },
        recovery: std::time::Duration::ZERO,
        answer: KeyframeAnswer::Idr,
        idr_pct: DEFAULT_IDR_PCT,
        bringup: std::time::Duration::from_secs(2),
        serve_ramp: true,
    });
    let (client, host) =
        synthetic_session(19788, source, |p| punktfunk_core::client::ConnectParams {
            probe_only: true,
            ..p
        });
    let r = health::health_check(&client, |_| {}).expect("the check reports");
    assert!(r.speed.clean.is_some(), "a ramp host gets a clean round");
    assert_eq!(
        r.legs.iter().map(|l| l.shape).collect::<Vec<_>>(),
        vec![LegShape::FrameBursts, LegShape::Capped]
    );
    for leg in &r.legs {
        assert!(
            leg.outcome.done && leg.outcome.wire_packets_sent > 0,
            "{leg:?}"
        );
    }
    assert!(r.host.sndbuf_kb > 0, "the host's facts arrived");
    assert!(r.client.rcvbuf_kb > 0, "the client read its own grant");
    drop(client);
    host.join().unwrap().unwrap();
}

/// Toward a host without a ramp the speed test is the single blast, and says nothing
/// about loss: there is no clean round to say it with.
#[test]
fn a_host_without_a_ramp_keeps_the_single_burst() {
    let _registry = crate::session_status::tests::registry_lock();
    let _serial = SESSION_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    use punktfunk_core::client::health;
    let (client, host) = synthetic_session(19785, Punktfunk1Source::Synthetic, |p| p);
    assert_eq!(
        client.host_caps2() & punktfunk_core::quic::HOST_CAP2_RAMP,
        0,
        "the plain synthetic source serves no ramp"
    );
    let r = health::speed_test(&client, |_| {}).expect("the blast reports");
    assert!(r.clean.is_none(), "no ramp, no clean round");
    assert!(!r.wall);
    let blast = r.blast.expect("the blast's own reading stands");
    assert!(blast.done && blast.wire_packets_sent > 0);
    assert_eq!(r.ceiling_kbps, blast.throughput_kbps);
    drop(client);
    host.join().unwrap().unwrap();
}

/// Toward a host that serves the ramp, the ceiling is what the ramp proved and the clean
/// round runs at half of it, with its own loss and jitter.
#[test]
fn the_clean_round_runs_under_the_ceiling_over_a_session() {
    let _registry = crate::session_status::tests::registry_lock();
    let _serial = SESSION_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    use punktfunk_core::client::health;
    let source = Punktfunk1Source::SyntheticAbr(SynthAbrShape {
        content: Content::Steady { fill_pct: 100 },
        recovery: std::time::Duration::ZERO,
        answer: KeyframeAnswer::Idr,
        idr_pct: DEFAULT_IDR_PCT,
        bringup: std::time::Duration::from_secs(2),
        serve_ramp: true,
    });
    let (client, host) = synthetic_session(19786, source, |p| p);
    assert_ne!(
        client.host_caps2() & punktfunk_core::quic::HOST_CAP2_RAMP,
        0
    );
    let mut polls = 0u32;
    let r = health::speed_test(&client, |_| polls += 1).expect("the round reports");
    let clean = r.clean.expect("a ramp host gets a clean round");
    assert!(r.ceiling_kbps > 0, "the ramp proved a rate");
    assert_eq!(clean.rate_kbps, health::clean_rate_kbps(r.ceiling_kbps));
    // The figure itself is not asserted: a loopback ramp proves gigabits, and at that
    // rate this process's own receive buffer drops — the round measures the path it is
    // given.
    assert!(clean.outcome.done && clean.outcome.wire_packets_sent > 0);
    assert!(clean.outcome.recv_packets > 0);
    assert!(r.blast.is_none());
    assert!(polls > 0, "the round reported its progress");
    drop(client);
    host.join().unwrap().unwrap();
}

/// Unpaired knock is parked; approve while waiting admits the same connection, no reconnect.
#[test]
fn delegated_approval_admits_after_knock() {
    let _registry = crate::session_status::tests::registry_lock();
    let _serial = SESSION_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    use punktfunk_core::client::{ConnectParams, NativeClient};
    use punktfunk_core::quic::endpoint;

    let store = crate::test_support::scratch().join("pf-approval-test.json");
    let np = Arc::new(NativePairing::load_with(Some(store.clone()), None, false).unwrap());
    let np_host = np.clone();
    let host = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(serve(
            Punktfunk1Options {
                port: 19779,
                source: Punktfunk1Source::Synthetic,
                seconds: 0,
                frames: 25,
                max_sessions: 1,
                max_concurrent: 1,
                require_pairing: true,
                allow_pairing: false,
                pairing_pin: None,
                paired_store: None,
                idle_timeout: None,
                mdns: false,
            },
            0,
            np_host,
            test_profiles(),
            StatsRecorder::new(
                std::env::temp_dir().join(format!("pf-approval-stats-{}", std::process::id())),
            ),
            crate::identity::ephemeral().unwrap(),
            None,
        ))
    });
    std::thread::sleep(std::time::Duration::from_millis(500));
    let (cert, key) = endpoint::generate_identity().unwrap();
    let expected_fp = hex::encode(endpoint::fingerprint_of_pem(&cert).unwrap());
    let mode = punktfunk_core::Mode {
        width: 1280,
        height: 720,
        refresh_hz: 60,
    };

    // Approve while the client is still parked.
    let np_approve = np.clone();
    let expect_fp = expected_fp.clone();
    let approver = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
        let pend = loop {
            if let Some(p) = np_approve
                .pending()
                .into_iter()
                .find(|p| p.fingerprint == expect_fp)
            {
                break p;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the knock must register while the client is parked"
            );
            std::thread::sleep(std::time::Duration::from_millis(40));
        };
        assert!(
            pend.name.starts_with("device "),
            "no Hello name → fingerprint-derived label, got {:?}",
            pend.name
        );
        // Past the client's own 2 s dial timeout.
        std::thread::sleep(std::time::Duration::from_millis(2500));
        np_approve
            .approve_pending(pend.id, Some("Approved Device"), None)
            .unwrap()
            .paired()
            .expect("pending id must approve");
    });

    // One connect that parks until approved, then streams. The client hears the host's
    // `Pending` and waits past its own 2 s dial timeout for the verdict. No Hello name: assert
    // the fingerprint-derived label. TOFU: approval, not a PIN, authorizes this client.
    let told = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let told_hook = told.clone();
    let client = NativeClient::connect(ConnectParams {
        identity: Some((cert, key)),
        on_pending: Some(std::sync::Arc::new(move || {
            told_hook.store(true, std::sync::atomic::Ordering::SeqCst)
        })),
        ..ConnectParams::new("127.0.0.1", 19779, mode, std::time::Duration::from_secs(2))
    })
    .expect("approved mid-park → session admitted with no reconnect");
    approver.join().unwrap();
    assert!(
        told.load(std::sync::atomic::Ordering::SeqCst),
        "the client heard the host's Pending"
    );
    assert!(
        np.is_paired(&expected_fp),
        "approval must pin the knocking fingerprint"
    );
    assert_eq!(np.list()[0].name, "Approved Device");
    // Hook filters match `client.connected` by name, so it must carry the approval rename.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let connected_name = loop {
        let found = crate::events::bus()
            .subscribe(0)
            .catch_up
            .into_iter()
            .find_map(|e| match e.kind {
                crate::events::EventKind::ClientConnected { client }
                    if client.fingerprint.as_deref() == Some(expected_fp.as_str()) =>
                {
                    Some(client.name)
                }
                _ => None,
            });
        if let Some(name) = found {
            break name;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "client.connected must fire for the approved device"
        );
        std::thread::sleep(std::time::Duration::from_millis(40));
    };
    assert_eq!(connected_name, "Approved Device");
    drop(client);
    let _ = std::fs::remove_file(&store);
    host.join().unwrap().unwrap();
}

/// An access request parks like a knock, and approval answers it without a session. Once
/// approved, the next request is answered at once.
#[test]
fn access_request_pairs_without_a_session() {
    let _registry = crate::session_status::tests::registry_lock();
    let _serial = SESSION_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    use punktfunk_core::client::NativeClient;
    use punktfunk_core::quic::endpoint;

    let store = crate::test_support::scratch().join("pf-access-test.json");
    let np = Arc::new(NativePairing::load_with(Some(store.clone()), None, false).unwrap());
    let np_host = np.clone();
    // A request names the fingerprint the host must show, as an advert would give it.
    let identity = crate::identity::ephemeral().unwrap();
    let host_pin = endpoint::fingerprint_of_pem(&identity.cert_pem).unwrap();
    let host = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(serve(
            Punktfunk1Options {
                port: 19795,
                source: Punktfunk1Source::Synthetic,
                seconds: 0,
                frames: 25,
                max_sessions: 2,
                max_concurrent: 1,
                require_pairing: true,
                allow_pairing: false,
                pairing_pin: None,
                paired_store: None,
                idle_timeout: None,
                mdns: false,
            },
            0,
            np_host,
            test_profiles(),
            StatsRecorder::new(
                std::env::temp_dir().join(format!("pf-access-stats-{}", std::process::id())),
            ),
            identity,
            None,
        ))
    });
    std::thread::sleep(std::time::Duration::from_millis(500));
    let (cert, key) = endpoint::generate_identity().unwrap();
    let expected_fp = hex::encode(endpoint::fingerprint_of_pem(&cert).unwrap());

    let np_approve = np.clone();
    let expect_fp = expected_fp.clone();
    let approver = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
        let pend = loop {
            if let Some(p) = np_approve
                .pending()
                .into_iter()
                .find(|p| p.fingerprint == expect_fp)
            {
                break p;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the request must park"
            );
            std::thread::sleep(std::time::Duration::from_millis(40));
        };
        assert_eq!(pend.name, "Access Deck");
        np_approve
            .approve_pending(pend.id, None, None)
            .unwrap()
            .paired()
            .expect("pending id must approve");
    });
    let ask = || {
        NativeClient::request_access(
            "127.0.0.1",
            19795,
            (&cert, &key),
            host_pin,
            "Access Deck",
            std::time::Duration::from_secs(15),
            None,
        )
    };
    let host_fp = ask().expect("approval answers the parked request");
    approver.join().unwrap();
    assert_eq!(host_fp, host_pin);
    assert!(np.is_paired(&expected_fp), "approval pairs the device");

    let started = std::time::Instant::now();
    assert_eq!(ask().expect("an approved device is answered"), host_fp);
    assert!(started.elapsed() < std::time::Duration::from_secs(5));

    // A session started after the answer would connect a moment later: give it the time.
    std::thread::sleep(std::time::Duration::from_millis(500));
    let connected = crate::events::bus()
        .subscribe(0)
        .catch_up
        .into_iter()
        .any(|e| match e.kind {
            crate::events::EventKind::ClientConnected { client } => {
                client.fingerprint.as_deref() == Some(expected_fp.as_str())
            }
            _ => false,
        });
    assert!(!connected, "an access request never starts a session");
    let _ = std::fs::remove_file(&store);
    host.join().unwrap().unwrap();
}

/// Right PIN pairs; paired identity gets a session; anonymous does not.
#[test]
fn pairing_ceremony_and_gate() {
    let _registry = crate::session_status::tests::registry_lock();
    let _serial = SESSION_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    use punktfunk_core::client::{ConnectParams, NativeClient};
    use punktfunk_core::quic::endpoint;

    let store = crate::test_support::scratch().join("punktfunk-paired-test.json");
    let paired_store = Some(store.clone());
    let host = std::thread::spawn(move || {
        run_ephemeral(Punktfunk1Options {
            port: 19778,
            source: Punktfunk1Source::Synthetic,
            seconds: 0,
            frames: 25,
            max_sessions: 4,
            max_concurrent: 1,
            require_pairing: true,
            allow_pairing: false,
            pairing_pin: Some("4321".into()),
            paired_store,
            idle_timeout: None,
            mdns: false,
        })
    });
    std::thread::sleep(std::time::Duration::from_millis(500));
    let timeout = std::time::Duration::from_secs(10);
    let (cert, key) = endpoint::generate_identity().unwrap();
    let identity = (cert.as_str(), key.as_str());
    let mode = punktfunk_core::Mode {
        width: 1280,
        height: 720,
        refresh_hz: 60,
    };

    // 1: anonymous session on a pairing-required host → rejected.
    assert!(
        NativeClient::connect(ConnectParams::new("127.0.0.1", 19778, mode, timeout)).is_err(),
        "anonymous session must be rejected"
    );

    // 2: correct PIN → paired. The one online attempt consumes the window (step 4).
    let host_fp = NativeClient::pair("127.0.0.1", 19778, identity, "4321", "test-client", timeout)
        .expect("pairing with the right PIN");
    assert!(store.exists());

    // 3: paired identity gets a session, pinned to the ceremony fingerprint.
    let client = NativeClient::connect(ConnectParams {
        pin: Some(host_fp),
        identity: Some((cert.clone(), key.clone())),
        ..ConnectParams::new("127.0.0.1", 19778, mode, timeout)
    })
    .expect("paired session");
    assert_eq!(client.host_fingerprint, host_fp);
    // Welcome reports a concrete backend. Do not pin which: `PUNKTFUNK_GAMEPAD` may be set.
    assert_ne!(client.resolved_gamepad, GamepadPref::Auto);
    drop(client);

    // 4: single-use PIN — a second attempt (even correct) is rejected.
    std::thread::sleep(PAIRING_COOLDOWN + std::time::Duration::from_millis(200));
    assert!(
        NativeClient::pair("127.0.0.1", 19778, identity, "4321", "too-late", timeout).is_err(),
        "the PIN window must be single-use (one online guess)"
    );

    host.join().unwrap().unwrap();
}

/// A stranger trying a PIN every second holds only the strict clock: an attempt from the home
/// network is still answered. Both planes charge through [`PairingClocks`].
#[test]
fn strict_attempts_leave_the_local_pairing_clock_free() {
    use crate::native_pairing::KnockSource;
    let clocks = PairingClocks::default();
    assert!(clocks.charge(KnockSource::Wan));
    assert!(
        !clocks.charge(KnockSource::Wan),
        "strict attempts share a clock"
    );
    assert!(
        clocks.charge(KnockSource::Lan),
        "a local attempt runs on its own"
    );
    assert!(!clocks.charge(KnockSource::Lan));
}

/// Controller-only passes pads only; View-only passes nothing. Classify is pinned in core.
#[test]
fn input_admission_matrix() {
    use punktfunk_core::quic::{GRANT_PRESET_CONTROLLER_ONLY, GRANT_PRESET_VIEW_ONLY};
    let admitted = |mask: u32, kind: InputKind| mask & classify(kind).bit() != 0;

    for kind in [
        InputKind::GamepadButton,
        InputKind::GamepadAxis,
        InputKind::GamepadState,
        InputKind::GamepadRemove,
        InputKind::GamepadArrival,
    ] {
        assert!(admitted(GRANT_PRESET_CONTROLLER_ONLY, kind), "{kind:?}");
        assert!(!admitted(GRANT_PRESET_VIEW_ONLY, kind), "{kind:?}");
    }
    for kind in [
        InputKind::KeyDown,
        InputKind::KeyUp,
        InputKind::MouseMove,
        InputKind::MouseMoveAbs,
        InputKind::MouseScroll,
        InputKind::Scroll,
        InputKind::TouchDown,
    ] {
        assert!(!admitted(GRANT_PRESET_CONTROLLER_ONLY, kind), "{kind:?}");
        assert!(!admitted(GRANT_PRESET_VIEW_ONLY, kind), "{kind:?}");
    }
    assert!(admitted(GRANT_ALL, InputKind::KeyDown));
}

/// Pairing-required synthetic host sharing `np` so the test can edit the store live.
/// Generous `frames`; the typed close cuts the stream.
fn spawn_access_host(
    port: u16,
    max_sessions: u32,
    np: Arc<NativePairing>,
) -> std::thread::JoinHandle<Result<()>> {
    spawn_profile_host(port, max_sessions, np, test_profiles())
}

/// [`spawn_access_host`] over the given profile store.
fn spawn_profile_host(
    port: u16,
    max_sessions: u32,
    np: Arc<NativePairing>,
    profiles: Arc<crate::profiles::Profiles>,
) -> std::thread::JoinHandle<Result<()>> {
    spawn_logged_host(port, max_sessions, np, profiles, None)
}

/// [`spawn_profile_host`] whose every thread logs to `logs`, when given.
fn spawn_logged_host(
    port: u16,
    max_sessions: u32,
    np: Arc<NativePairing>,
    profiles: Arc<crate::profiles::Profiles>,
    logs: Option<tracing::Dispatch>,
) -> std::thread::JoinHandle<Result<()>> {
    std::thread::spawn(move || {
        let on_each = logs.clone();
        let _logs = logs.map(|d| tracing::dispatcher::set_default(&d));
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .on_thread_start(move || {
                if let Some(d) = &on_each {
                    // The guard lives as long as the runtime's thread.
                    std::mem::forget(tracing::dispatcher::set_default(d));
                }
            })
            .build()
            .unwrap();
        rt.block_on(serve(
            Punktfunk1Options {
                port,
                source: Punktfunk1Source::Synthetic,
                seconds: 0,
                frames: 3000, // ~50 s at 60 fps; the stop flag cuts it long before
                max_sessions,
                max_concurrent: 1,
                require_pairing: true,
                allow_pairing: false,
                pairing_pin: None,
                paired_store: None,
                idle_timeout: None,
                mdns: false,
            },
            0,
            np,
            profiles,
            StatsRecorder::new(
                std::env::temp_dir().join(format!("pf-access-stats-{port}-{}", std::process::id())),
            ),
            crate::identity::ephemeral().unwrap(),
            None,
        ))
    })
}

/// Paired-store temp path; the shared-`np` hosts persist through it.
fn access_store_path(tag: &str) -> std::path::PathBuf {
    crate::test_support::scratch().join(format!("pf-access-{tag}.json"))
}

/// `ClientHello` → `ServerHello` → `Ready`, returning streams so a test can read
/// `AccessUpdate`s and the exact close code. The media handle keeps the client's socket open.
async fn raw_session(
    port: u16,
    identity: (&str, &str),
) -> (
    quinn::Connection,
    quinn::SendStream,
    punktfunk_core::quic::v2::io::FrameReader<quinn::RecvStream>,
    Welcome,
    punktfunk_core::transport::shared::ClientMedia,
) {
    use punktfunk_core::quic::v2::hello::{Ready, ServerHello};
    use punktfunk_core::quic::v2::{io as v2io, msg, registry};
    let (ep, _observed) = endpoint::client_shared(None, Some(identity), &[registry::ALPN], None);
    let (ep, media) = ep.expect("client endpoint");
    let conn = ep
        .connect(format!("127.0.0.1:{port}").parse().unwrap(), "punktfunk")
        .expect("connect")
        .await
        .expect("QUIC handshake");
    let (mut send, recv) = conn.open_bi().await.expect("control stream");
    v2io::write_stream_type(&mut send, registry::STREAM_CONTROL)
        .await
        .expect("stream type");
    let mut recv = v2io::FrameReader::new(recv);
    v2io::send(&mut send, &plain_hello(None, Default::default()))
        .await
        .expect("ClientHello");
    let welcome = loop {
        let (ty, body) = recv.read_frame().await.expect("ServerHello read");
        if ty != msg::Pending::TYPE {
            break msg::decode::<ServerHello>(ty, &body)
                .expect("ServerHello")
                .welcome;
        }
    };
    v2io::send(&mut send, &Ready {}).await.expect("Ready");
    (conn, send, recv, welcome, media)
}

/// A 720p60 `ClientHello` asking for `profile`, with `features`.
fn plain_hello(
    profile: Option<&str>,
    features: punktfunk_core::quic::v2::features::FeatureSet,
) -> ClientHello {
    let hello = Hello {
        mode: punktfunk_core::Mode {
            width: 1280,
            height: 720,
            refresh_hz: 60,
        },
        compositor: CompositorPref::Auto,
        gamepad: GamepadPref::Auto,
        bitrate_kbps: 0,
        name: Some("access-test".into()),
        launch: None,
        video_caps: 0,
        audio_channels: 2,
        video_codecs: 0,
        preferred_codec: 0,
        display_hdr: None,
        client_caps: 0,
        max_shard_payload: 0,
        audio_rate_hz: punktfunk_core::audio::SAMPLE_RATE_HZ,
        audio_bits: punktfunk_core::audio::pcm::BITS_16,
        audio_layout: 0,
        video_fit: 0,
        audio_bitstreams: 0,
    };
    ClientHello {
        hello,
        client_label: None,
        abr_features: 0,
        preset: None,
        link: Default::default(),
        probe_only: false,
        pyrowave_bpp_x100: 0,
        slices: 0,
        resume: None,
        suites: Vec::new(),
        features,
        profile: profile.map(Into::into),
        access_only: false,
        screens: Vec::new(),
        view: None,
    }
}

/// Application close code. Panics on a transport-level end — these tests expect a host close.
async fn closed_app_code(conn: &quinn::Connection) -> u32 {
    match conn.closed().await {
        quinn::ConnectionError::ApplicationClosed(ac) => {
            u32::try_from(u64::from(ac.error_code)).expect("close code fits u32")
        }
        other => panic!("expected an application close, got {other:?}"),
    }
}

/// A client from before `punktfunk/2` still pairs by PIN over `pkf1`, and is closed with the
/// wire-version code when it dials anything else there.
#[test]
fn a_pkf1_client_pairs_and_is_told_to_update() {
    let _registry = crate::session_status::tests::registry_lock();
    let _serial = SESSION_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    use punktfunk_core::quic::{endpoint, pake, PairChallenge, PairProof, PairResult};

    let store = access_store_path("pkf1-pair");
    let _ = std::fs::remove_file(&store);
    let host = std::thread::spawn({
        let store = store.clone();
        move || {
            run_ephemeral(Punktfunk1Options {
                port: 19784,
                source: Punktfunk1Source::Synthetic,
                seconds: 0,
                frames: 25,
                max_sessions: 2,
                max_concurrent: 1,
                require_pairing: true,
                allow_pairing: false,
                pairing_pin: Some("2468".into()),
                paired_store: Some(store),
                idle_timeout: None,
                mdns: false,
            })
        }
    });
    std::thread::sleep(std::time::Duration::from_millis(500));
    let (cert, key) = endpoint::generate_identity().unwrap();
    let client_fp = endpoint::fingerprint_of_pem(&cert).unwrap();
    let addr: std::net::SocketAddr = "127.0.0.1:19784".parse().unwrap();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let (ep, observed) = endpoint::client_pinned_offering(
            None,
            Some((cert.as_str(), key.as_str())),
            &[endpoint::QUIC_ALPN],
        );
        let ep = ep.expect("client endpoint");

        // The PIN ceremony in `punktfunk/1`'s framing, as such a client runs it.
        let conn = ep.connect(addr, "punktfunk").unwrap().await.unwrap();
        let host_fp = observed.lock().unwrap().expect("the host's certificate");
        let (mut send, mut recv) = conn.open_bi().await.unwrap();
        let (spake, spake_a) = pake::start(true, "2468", &client_fp, &host_fp);
        let req = PairRequest {
            name: "older client".into(),
            spake_a,
            device_key: Vec::new(),
        };
        pkf1::write(&mut send, &req.encode_pkf1()).await.unwrap();
        let challenge = PairChallenge::decode_pkf1(&pkf1::read(&mut recv).await.unwrap()).unwrap();
        let confirms = spake.finish(&challenge.spake_b).unwrap();
        assert!(pake::verify(&confirms.host, &challenge.confirm));
        let proof = PairProof {
            confirm: confirms.client,
        };
        pkf1::write(&mut send, &proof.encode_pkf1()).await.unwrap();
        let result = PairResult::decode_pkf1(&pkf1::read(&mut recv).await.unwrap()).unwrap();
        assert!(result.ok, "the pairing completes");
        conn.close(0u32.into(), b"pair done");

        // Its session dial is told to update. A v1 Hello opens with `PKF1`.
        let conn = ep.connect(addr, "punktfunk").unwrap().await.unwrap();
        let (mut send, _recv) = conn.open_bi().await.unwrap();
        pkf1::write(&mut send, b"PKF1\x01\x00\x00\x00")
            .await
            .unwrap();
        let code = tokio::time::timeout(std::time::Duration::from_secs(5), closed_app_code(&conn))
            .await
            .expect("the host closes the dial");
        assert_eq!(code, punktfunk_core::reject::WIRE_VERSION_CLOSE_CODE);
    });
    assert!(
        std::fs::read_to_string(&store).is_ok_and(|s| s.contains("older client")),
        "the paired device is stored"
    );
    let _ = std::fs::remove_file(&store);
    host.join().unwrap().unwrap();
}

/// Short expiry: Welcome advertises grants + remaining; deadline closes typed (`0x69`).
#[test]
fn access_expiry_advertises_and_closes_typed() {
    let _registry = crate::session_status::tests::registry_lock();
    let _serial = SESSION_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    use punktfunk_core::quic::endpoint;

    let store = access_store_path("expiry");
    let _ = std::fs::remove_file(&store);
    let np = Arc::new(NativePairing::load_with(Some(store.clone()), None, false).unwrap());
    let (cert, key) = endpoint::generate_identity().unwrap();
    let fp_hex = hex::encode(endpoint::fingerprint_of_pem(&cert).unwrap());
    let host = spawn_access_host(19782, 1, np.clone());
    std::thread::sleep(std::time::Duration::from_millis(500));
    // Granted once the host is up: a handshake that reaches the pairing gate after expiry is
    // parked as an unpaired knock. 4 s leaves at least 3 s for it on a loaded runner.
    np.add_with_access(
        "Evening Guest",
        &fp_hex,
        Some(crate::native_pairing::Access {
            grants: GRANT_ALL,
            expires_unix: Some(crate::clock::unix_secs() + 4),
            until_disconnect: false,
        }),
    )
    .unwrap();

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let (conn, _send, _recv, welcome, _media) =
            raw_session(19782, (cert.as_str(), key.as_str())).await;
        assert_eq!(welcome.grants, GRANT_ALL, "the Welcome advertises the mask");
        assert!(
            (1..=4).contains(&welcome.expires_in_secs),
            "a 4 s grant must advertise 1–4 remaining secs, got {}",
            welcome.expires_in_secs
        );
        let code = tokio::time::timeout(std::time::Duration::from_secs(10), closed_app_code(&conn))
            .await
            .expect("the deadline task must close the session");
        assert_eq!(
            code,
            punktfunk_core::reject::ACCESS_EXPIRED_CLOSE_CODE,
            "expiry must close with the typed code"
        );
    });
    // The row survives expiry — only authorization ends.
    assert!(np.is_paired(&fp_hex));
    assert_eq!(np.effective(&fp_hex, crate::clock::unix_secs()), None);
    let _ = std::fs::remove_file(&store);
    host.join().unwrap().unwrap();
}

/// Mid-session grant edit → `AccessUpdate`; T−1 m warning fires; "expire now" typed-closes.
#[test]
fn access_edit_pushes_updates_and_expire_now_closes() {
    let _registry = crate::session_status::tests::registry_lock();
    let _serial = SESSION_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    use punktfunk_core::quic::endpoint;

    let store = access_store_path("edit");
    let _ = std::fs::remove_file(&store);
    let np = Arc::new(NativePairing::load_with(Some(store.clone()), None, false).unwrap());
    let (cert, key) = endpoint::generate_identity().unwrap();
    let fp_hex = hex::encode(endpoint::fingerprint_of_pem(&cert).unwrap());
    np.add("Edited Device", &fp_hex).unwrap();
    let host = spawn_access_host(19783, 1, np.clone());
    std::thread::sleep(std::time::Duration::from_millis(500));

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let (conn, _send, mut recv, welcome, _media) =
            raw_session(19783, (cert.as_str(), key.as_str())).await;
        assert_eq!(welcome.grants, GRANT_ALL);
        assert_eq!(welcome.expires_in_secs, 0, "permanent access advertises 0");
        // The control task opens with the session's first config.
        let (ty, body) = tokio::time::timeout(std::time::Duration::from_secs(5), recv.read_frame())
            .await
            .expect("the first StreamConfig owed")
            .expect("control stream open");
        punktfunk_core::quic::v2::msg::decode::<punktfunk_core::quic::v2::msg::StreamConfig>(
            ty, &body,
        )
        .expect("a StreamConfig");

        // Controller-only, 62 s out (inside T−5 m, outside T−1 m): one warning, ~2 s later.
        let now = crate::clock::unix_secs();
        np.set_access(
            &fp_hex,
            crate::native_pairing::Access {
                grants: punktfunk_core::quic::GRANT_PRESET_CONTROLLER_ONLY,
                expires_unix: Some(now + 62),
                until_disconnect: false,
            },
        )
        .unwrap()
        .then_some(())
        .expect("the fingerprint is paired");

        // Update 1: the edit itself (new mask + remaining).
        let (ty, body) = tokio::time::timeout(std::time::Duration::from_secs(5), recv.read_frame())
            .await
            .expect("edit AccessUpdate owed")
            .expect("control stream open");
        let u = punktfunk_core::quic::v2::msg::decode::<AccessUpdate>(ty, &body)
            .expect("an AccessUpdate");
        assert_eq!(u.grants, punktfunk_core::quic::GRANT_PRESET_CONTROLLER_ONLY);
        assert!(
            (55..=62).contains(&u.remaining_secs),
            "remaining should track the fresh deadline, got {}",
            u.remaining_secs
        );

        // Update 2: T−1 m warning, fired as the threshold is crossed live.
        let (ty, body) =
            tokio::time::timeout(std::time::Duration::from_secs(10), recv.read_frame())
                .await
                .expect("T-1m warning owed")
                .expect("control stream open");
        let u = punktfunk_core::quic::v2::msg::decode::<AccessUpdate>(ty, &body)
            .expect("an AccessUpdate");
        assert!(
            u.remaining_secs <= 60,
            "the warning carries the crossed threshold, got {}",
            u.remaining_secs
        );

        // Expire now: deadline in the past → typed close, no phantom update.
        np.set_access(
            &fp_hex,
            crate::native_pairing::Access {
                grants: punktfunk_core::quic::GRANT_PRESET_CONTROLLER_ONLY,
                expires_unix: Some(crate::clock::unix_secs() - 1),
                until_disconnect: false,
            },
        )
        .unwrap();
        let code = tokio::time::timeout(std::time::Duration::from_secs(10), closed_app_code(&conn))
            .await
            .expect("expire-now must close the session");
        assert_eq!(code, punktfunk_core::reject::ACCESS_EXPIRED_CLOSE_CODE);
    });
    let _ = std::fs::remove_file(&store);
    host.join().unwrap().unwrap();
}

/// Launch without the grant: typed 0x6A before handshake. Same device without launch is admitted.
#[test]
fn launch_refused_without_grant_but_session_admitted() {
    let _registry = crate::session_status::tests::registry_lock();
    let _serial = SESSION_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    use punktfunk_core::client::{ConnectParams, NativeClient};
    use punktfunk_core::quic::endpoint;

    let store = access_store_path("launch");
    let _ = std::fs::remove_file(&store);
    let np = Arc::new(NativePairing::load_with(Some(store.clone()), None, false).unwrap());
    let (cert, key) = endpoint::generate_identity().unwrap();
    let fp_hex = hex::encode(endpoint::fingerprint_of_pem(&cert).unwrap());
    np.add_with_access(
        "Guest Pad",
        &fp_hex,
        Some(crate::native_pairing::Access {
            grants: punktfunk_core::quic::GRANT_PRESET_CONTROLLER_ONLY,
            expires_unix: None,
            until_disconnect: false,
        }),
    )
    .unwrap();
    // max_sessions counts accepted connections; the refused launch connect is one too.
    let host = spawn_access_host(19784, 2, np.clone());
    std::thread::sleep(std::time::Duration::from_millis(500));
    let timeout = std::time::Duration::from_secs(10);
    let mode = punktfunk_core::Mode {
        width: 1280,
        height: 720,
        refresh_hz: 60,
    };

    // 1: launch without LAUNCH → typed pre-handshake refusal (`NativeClient` has no Debug).
    let refused = NativeClient::connect(ConnectParams {
        launch: Some("steam:570".into()),
        name: Some("Guest Pad".into()),
        identity: Some((cert.clone(), key.clone())),
        ..ConnectParams::new("127.0.0.1", 19784, mode, timeout)
    });
    match refused {
        Ok(_) => panic!("a launch without the grant must be refused"),
        Err(punktfunk_core::PunktfunkError::Rejected(r, _)) => assert_eq!(
            r,
            punktfunk_core::reject::RejectReason::LaunchNotPermitted,
            "the refusal must carry the typed launch reason"
        ),
        Err(other) => panic!("expected a typed rejection, got {other:?}"),
    }

    // 2: same device without a launch is admitted.
    let client = NativeClient::connect(ConnectParams {
        name: Some("Guest Pad".into()),
        identity: Some((cert, key)),
        ..ConnectParams::new("127.0.0.1", 19784, mode, timeout)
    })
    .expect("controller-only session without a launch must be admitted");
    drop(client);
    let _ = std::fs::remove_file(&store);
    host.join().unwrap().unwrap();
}

/// The asked profile is echoed, no ask lands on the owner, and an unknown id is refused
/// before anything is built for it.
#[test]
fn a_profile_is_resolved_in_the_handshake() {
    let _registry = crate::session_status::tests::registry_lock();
    let _serial = SESSION_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    use punktfunk_core::client::{ConnectParams, NativeClient};
    use punktfunk_core::quic::endpoint;

    let store = access_store_path("profiles");
    let _ = std::fs::remove_file(&store);
    let np = Arc::new(NativePairing::load_with(Some(store.clone()), None, false).unwrap());
    let (cert, key) = endpoint::generate_identity().unwrap();
    let fp_hex = hex::encode(endpoint::fingerprint_of_pem(&cert).unwrap());
    np.add_with_access("Couch", &fp_hex, None).unwrap();
    let file = crate::test_support::scratch().join("pf-handshake-profiles.json");
    std::fs::write(
        &file,
        br#"{"version":1,"profiles":[
            {"id":"4f1c3a9b0e27","display_name":"Enrico","os_account":{"kind":"operator"}},
            {"id":"9a3f1c2b7e40","display_name":"Kid","os_account":{"kind":"seat"}}]}"#,
    )
    .unwrap();
    let profiles = Arc::new(crate::profiles::Profiles::load_with(
        Some(file.clone()),
        None,
    ));
    let host = spawn_profile_host(19794, 3, np, profiles);
    std::thread::sleep(std::time::Duration::from_millis(500));
    let dial = |profile: Option<&str>| {
        NativeClient::connect(ConnectParams {
            name: Some("Couch".into()),
            identity: Some((cert.clone(), key.clone())),
            profile: profile.map(Into::into),
            ..ConnectParams::new(
                "127.0.0.1",
                19794,
                punktfunk_core::Mode {
                    width: 1280,
                    height: 720,
                    refresh_hz: 60,
                },
                std::time::Duration::from_secs(10),
            )
        })
    };

    let kid = dial(Some("9a3f1c2b7e40")).expect("a known profile is admitted");
    assert_eq!(kid.profile(), Some("9a3f1c2b7e40"));
    drop(kid);
    let owner = dial(None).expect("no ask is admitted");
    assert_eq!(owner.profile(), Some("4f1c3a9b0e27"));
    drop(owner);
    match dial(Some("ffffffffffff")) {
        Ok(_) => panic!("an unknown profile must be refused"),
        Err(punktfunk_core::PunktfunkError::Rejected(r, _)) => {
            assert_eq!(r, punktfunk_core::reject::RejectReason::ProfileUnknown)
        }
        Err(other) => panic!("expected a typed rejection, got {other:?}"),
    }
    let _ = std::fs::remove_file(&store);
    let _ = std::fs::remove_file(&file);
    host.join().unwrap().unwrap();
}

/// A launch the host cannot resolve streams on, and the client learns why from the
/// control message.
#[test]
fn unknown_launch_reaches_the_client_as_a_refusal() {
    let _registry = crate::session_status::tests::registry_lock();
    let _serial = SESSION_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    use punktfunk_core::client::{ConnectParams, NativeClient};
    use punktfunk_core::quic::{endpoint, LaunchOutcomeKind};

    let store = access_store_path("launch-outcome");
    let _ = std::fs::remove_file(&store);
    let np = Arc::new(NativePairing::load_with(Some(store.clone()), None, false).unwrap());
    let (cert, key) = endpoint::generate_identity().unwrap();
    let fp_hex = hex::encode(endpoint::fingerprint_of_pem(&cert).unwrap());
    np.add_with_access("Launcher", &fp_hex, None).unwrap();
    let host = spawn_access_host(19786, 1, np);
    std::thread::sleep(std::time::Duration::from_millis(500));
    let client = NativeClient::connect(ConnectParams {
        launch: Some("pf-test:no-such-title".into()),
        name: Some("Launcher".into()),
        identity: Some((cert, key)),
        ..ConnectParams::new(
            "127.0.0.1",
            19786,
            punktfunk_core::Mode {
                width: 1280,
                height: 720,
                refresh_hz: 60,
            },
            std::time::Duration::from_secs(10),
        )
    })
    .expect("an unresolvable launch still admits the session");
    // A cold library scan decides the refusal; it can take seconds.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    let outcome = loop {
        if let Some(o) = client.launch_outcome() {
            break o;
        }
        assert!(std::time::Instant::now() < deadline, "no launch outcome");
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    assert_eq!(outcome.kind, LaunchOutcomeKind::Refused);
    assert!(outcome
        .notice()
        .is_some_and(|n| n.starts_with("Couldn't start")));
    drop(client);
    let _ = std::fs::remove_file(&store);
    host.join().unwrap().unwrap();
}

/// Set on the refused-prep test's child only.
#[cfg(unix)]
const PREP_CHILD: &str = "PF_REFUSED_PREP_TEST_CHILD";

/// A title the session can't launch runs none of its prep: one the box hides, and one the
/// session's profile hides, are refused before a step runs. The library is the config dir,
/// process config, so the body runs in a child of this test binary with a config dir of its own.
#[cfg(unix)]
#[test]
fn a_refused_launch_runs_none_of_its_prep() {
    if std::env::var_os(PREP_CHILD).is_none() {
        let dir = tempfile::tempdir().unwrap();
        let module = module_path!().split_once("::").expect("crate::module").1;
        let test = format!("{module}::a_refused_launch_runs_none_of_its_prep");
        let mut child =
            std::process::Command::new(std::env::current_exe().expect("this test binary"));
        child.args(["--exact", &test, "--nocapture"]);
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("PUNKTFUNK_") {
                child.env_remove(key);
            }
        }
        let out = child
            .env(PREP_CHILD, "1")
            .env("PUNKTFUNK_CONFIG_DIR", dir.path())
            .output()
            .expect("run the child");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{stdout}{stderr}");
        assert!(
            stdout.contains("1 passed"),
            "the child ran no test: {stdout}"
        );
        return;
    }
    use crate::library::{set_entry_hidden, Reach, View};
    let dir = pf_paths::config_dir();
    let marker = |title: &str| dir.join(format!("{title}.prep"));
    let add = |title: &str| {
        let touch = format!("touch {}", marker(title).display());
        let input = serde_json::json!({"title": title, "prep": [{"do": touch}]});
        let entry = crate::library::add_custom(serde_json::from_value(input).unwrap()).unwrap();
        format!("custom:{}", entry.id)
    };
    let (boxed, own, shown) = (add("boxed"), add("own"), add("shown"));
    const KID: &str = "a1b2c3d4e5f6";
    set_entry_hidden(&View::here(), &boxed, true).unwrap();
    set_entry_hidden(&View::of(KID, false, Reach::default()), &own, true).unwrap();
    let resolved = |id: &str, owner: bool| crate::profiles::Resolved {
        id: id.into(),
        display_name: id.into(),
        os_account: crate::profiles::OsAccount::Operator,
        home: Default::default(),
        via: crate::profiles::ResolveVia::Asked,
        owner,
    };
    let owner = crate::session_launch::LaunchOwner {
        client: "prep-test".into(),
        fingerprint: None,
        plane: crate::events::Plane::Native,
        preset: None,
        profile: None,
        pad: punktfunk_core::config::GamepadPref::Xbox360,
        pad_slots: None,
    };
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    for (id, profile) in [
        (&boxed, resolved("0f0f0f0f0f0f", true)),
        (&own, resolved(KID, false)),
    ] {
        assert!(!crate::library::prep_for(id).is_empty(), "{id} has prep");
        let mut hello = plain_hello(None, Default::default()).hello;
        hello.launch = Some(id.clone());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let stop = std::sync::atomic::AtomicBool::new(false);
        let (target, _, prepared) = rt
            .block_on(super::launch::prepare_launch(
                &hello, false, &profile, &owner, &tx, &stop,
            ))
            .unwrap();
        assert!(target.is_none() && prepared.prep.is_none(), "{id}");
        let refusal = rx.try_recv().expect("a refusal");
        assert_eq!(
            refusal.kind,
            punktfunk_core::quic::LaunchOutcomeKind::Refused
        );
    }
    assert!(!marker("boxed").exists() && !marker("own").exists());
    // The same step runs for a title nobody hides.
    let _prep = crate::hooks::run_prep(&crate::library::prep_for(&shown), &[]);
    assert!(marker("shown").exists());
}

/// Expired record knocks into pending; re-approval is the re-grant on the held connection.
#[test]
fn expired_record_knocks_into_pending_and_reapproval_regrants() {
    let _registry = crate::session_status::tests::registry_lock();
    let _serial = SESSION_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    use punktfunk_core::client::{ConnectParams, NativeClient};
    use punktfunk_core::quic::endpoint;

    let store = access_store_path("regrant");
    let _ = std::fs::remove_file(&store);
    let np = Arc::new(NativePairing::load_with(Some(store.clone()), None, false).unwrap());
    let (cert, key) = endpoint::generate_identity().unwrap();
    let fp_hex = hex::encode(endpoint::fingerprint_of_pem(&cert).unwrap());
    // Still listed, no longer authorized.
    np.add_with_access(
        "Yesterday's Guest",
        &fp_hex,
        Some(crate::native_pairing::Access {
            grants: GRANT_ALL,
            expires_unix: Some(crate::clock::unix_secs() - 3600),
            until_disconnect: false,
        }),
    )
    .unwrap();
    assert!(np.is_paired(&fp_hex), "expired but still listed");
    assert_eq!(np.effective(&fp_hex, crate::clock::unix_secs()), None);

    let host = spawn_access_host(19785, 1, np.clone());
    std::thread::sleep(std::time::Duration::from_millis(500));

    // Reconnect appears as pending; approve with fresh access while parked.
    let np_approve = np.clone();
    let fp_approve = fp_hex.clone();
    let approver = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
        let pend = loop {
            if let Some(p) = np_approve
                .pending()
                .into_iter()
                .find(|p| p.fingerprint == fp_approve)
            {
                break p;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "an expired record's reconnect must knock into the pending list"
            );
            std::thread::sleep(std::time::Duration::from_millis(40));
        };
        np_approve
            .approve_pending(
                pend.id,
                None,
                Some(crate::native_pairing::Access {
                    grants: punktfunk_core::quic::GRANT_PRESET_CONTROLLER_ONLY,
                    expires_unix: Some(crate::clock::unix_secs() + 4 * 3600),
                    until_disconnect: false,
                }),
            )
            .unwrap()
            .paired()
            .expect("re-approval");
    });

    let client = NativeClient::connect(ConnectParams {
        name: Some("Yesterday's Guest".into()),
        identity: Some((cert, key)),
        ..ConnectParams::new(
            "127.0.0.1",
            19785,
            punktfunk_core::Mode {
                width: 1280,
                height: 720,
                refresh_hz: 60,
            },
            std::time::Duration::from_secs(15),
        )
    })
    .expect("re-approved mid-park → session admitted with no reconnect");
    approver.join().unwrap();
    // Re-grant in force: controller-only.
    assert_eq!(
        np.effective(&fp_hex, crate::clock::unix_secs()),
        Some(punktfunk_core::quic::GRANT_PRESET_CONTROLLER_ONLY)
    );
    drop(client);
    let _ = std::fs::remove_file(&store);
    host.join().unwrap().unwrap();
}

/// A redirected "this session" guest keeps its record while it plays on the seat. The hold
/// outlasts the dial and the picture bring-up, and ends when the guest has left, or never came,
/// or the seats have said nothing for too long.
#[test]
fn a_redirected_guest_is_held_until_it_leaves_the_seat() {
    let s = std::time::Duration::from_secs;
    let over = |here, landed, waited, silent| hold_over(here, landed, s(waited), s(silent));
    assert!(!over(Some(false), false, 10, 0), "still dialing in");
    assert!(!over(Some(true), true, 600, 0), "playing");
    assert!(over(Some(false), true, 600, 0), "left after playing");
    assert!(over(Some(false), false, 121, 0), "never came");
    assert!(!over(None, true, 600, 5), "a seat that blinked");
    assert!(over(None, true, 600, 61), "a seat that hangs");
}

/// The device whose next session fails at its launch, and that session's lifecycle once it did.
type Armed = Option<([u8; 32], Option<crate::lifecycle::Lifecycle>)>;
static FAIL_AT_LAUNCH: std::sync::Mutex<Armed> = std::sync::Mutex::new(None);

/// The session's injected failure: the armed device's session fails here, after its planes
/// started. Keyed by fingerprint, so no other test's session is touched.
pub(super) fn fail_at_launch(
    conn: &link::SessionLink,
    life: &crate::lifecycle::Lifecycle,
) -> Result<()> {
    let mut armed = FAIL_AT_LAUNCH.lock().unwrap_or_else(|p| p.into_inner());
    match armed.as_mut() {
        Some((fp, seen)) if conn.peer_fingerprint() == Some(*fp) => {
            *seen = Some(life.clone());
            anyhow::bail!("injected launch failure")
        }
        _ => Ok(()),
    }
}

/// A session that fails once its input thread and watchers run is torn down like any other:
/// ended as the host's error, its threads joined, and the client hears a typed setup failure.
#[test]
fn a_session_that_fails_after_its_planes_start() {
    let _registry = crate::session_status::tests::registry_lock();
    let _serial = SESSION_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    use crate::events::SessionEndReason;
    use crate::lifecycle::Phase;
    use punktfunk_core::quic::endpoint;

    let (cert, key) = endpoint::generate_identity().unwrap();
    let fp = endpoint::fingerprint_of_pem(&cert).unwrap();
    *FAIL_AT_LAUNCH.lock().unwrap_or_else(|p| p.into_inner()) = Some((fp, None));
    let host = std::thread::spawn(|| {
        run_ephemeral(Punktfunk1Options {
            port: 19796,
            source: Punktfunk1Source::Synthetic,
            seconds: 0,
            frames: 600,
            max_sessions: 1,
            max_concurrent: 1,
            require_pairing: false,
            allow_pairing: false,
            pairing_pin: None,
            paired_store: None,
            idle_timeout: None,
            mdns: false,
        })
    });
    std::thread::sleep(std::time::Duration::from_millis(500));
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let code = rt.block_on(async {
        let (conn, _send, _recv, _welcome, _media) =
            raw_session(19796, (cert.as_str(), key.as_str())).await;
        tokio::time::timeout(std::time::Duration::from_secs(10), closed_app_code(&conn))
            .await
            .expect("the host closes the failed session")
    });
    host.join().unwrap().unwrap();
    let life = FAIL_AT_LAUNCH
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .take()
        .and_then(|(_, life)| life)
        .expect("the session reached its launch");
    assert_eq!(code, punktfunk_core::reject::SETUP_FAILED_CLOSE_CODE);
    assert_eq!(life.reason(), Some(SessionEndReason::HostError));
    assert_eq!(life.phase(), Phase::Done, "the teardown joined its threads");
}

/// What a raw client dialling a person's own account sends after its hello.
enum Sends {
    Unlock(v2msg::ProfileUnlock),
    /// Announces an unlock, then sends another frame in its place.
    Garbled,
    Nothing,
}

/// What it hears back: a redirect, the session starting here, or the host's typed close and its
/// sentence.
#[derive(Debug, PartialEq)]
enum Heard {
    Redirect { port: u16, pin: String },
    Played,
    Closed(punktfunk_core::reject::RejectReason, String),
}

/// One dial for `profile`, offering `features`, then `sends`; the `Pending`s it heard and how
/// the host ended it.
async fn dial_person(
    port: u16,
    identity: (&str, &str),
    profile: &str,
    features: punktfunk_core::quic::v2::features::FeatureSet,
    sends: Sends,
) -> (usize, Heard) {
    use punktfunk_core::quic::v2::{io as v2io, msg, registry};
    let (ep, _observed) = endpoint::client_shared(None, Some(identity), &[registry::ALPN], None);
    let (ep, _media) = ep.expect("client endpoint");
    let conn = ep
        .connect(format!("127.0.0.1:{port}").parse().unwrap(), "punktfunk")
        .expect("connect")
        .await
        .expect("QUIC handshake");
    let (mut send, recv) = conn.open_bi().await.expect("control stream");
    v2io::write_stream_type(&mut send, registry::STREAM_CONTROL)
        .await
        .unwrap();
    let mut recv = v2io::FrameReader::new(recv);
    let features = match sends {
        Sends::Nothing => features,
        _ => features.with(registry::FEATURE_UNLOCK_FOLLOWS),
    };
    v2io::send(&mut send, &plain_hello(Some(profile), features))
        .await
        .unwrap();
    match sends {
        Sends::Unlock(unlock) => v2io::write_frame(&mut send, &unlock.encode())
            .await
            .unwrap(),
        Sends::Garbled => v2io::send(&mut send, &Ready {}).await.unwrap(),
        Sends::Nothing => {}
    }
    let mut pendings = 0;
    while let Ok((ty, body)) = recv.read_frame().await {
        if ty == msg::Pending::TYPE {
            pendings += 1;
            continue;
        }
        if ty == registry::MSG_SERVER_HELLO {
            conn.close(0u32.into(), b"");
            return (pendings, Heard::Played);
        }
        assert_eq!(
            ty,
            msg::Redirect::TYPE,
            "an admission answers no other frame"
        );
        let to = msg::decode::<msg::Redirect>(ty, &body).unwrap();
        conn.close(0u32.into(), b"");
        let (port, pin) = (to.port, to.pin);
        return (pendings, Heard::Redirect { port, pin });
    }
    let quinn::ConnectionError::ApplicationClosed(ac) = conn.closed().await else {
        panic!("the host closes the connect itself");
    };
    let code = u32::try_from(u64::from(ac.error_code)).unwrap();
    let reason = punktfunk_core::reject::RejectReason::from_close_code(code)
        .unwrap_or_else(|| panic!("close code {code:#x} is no typed refusal"));
    let sentence = String::from_utf8(ac.reason.to_vec()).unwrap();
    (pendings, Heard::Closed(reason, sentence))
}

const PERSON_SEAT: &str = "7a11ce007a11ce007a11ce007a11ce00";
const WINDOWS_SEAT: &str = "5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a";
const MADE_SEAT: &str = "3d3d3d3d3d3d3d3d3d3d3d3d3d3d3d3d";
const PERSON: &str = "7a11ce00e001";

/// The fake supervisor's ledger: a person's own Linux account, a Windows seat on an existing
/// account and a made seat, each running on its own port.
fn person_ledger() -> Vec<pf_seats::Seat> {
    let row = |id: &str, port: u16| {
        let mut seat = crate::native::unlock::fake::person(id, port);
        seat.adopted_uid = None;
        seat
    };
    let mut windows = row(WINDOWS_SEAT, 9781);
    windows.adopted_sid = Some("S-1-5-21-1-2-3-1001".into());
    vec![
        crate::native::unlock::fake::person(PERSON_SEAT, 9780),
        windows,
        row(MADE_SEAT, 9782),
    ]
}

/// A profile store with a profile on each of [`person_ledger`]'s rows.
fn person_profiles(tag: &str) -> Arc<crate::profiles::Profiles> {
    let file = crate::test_support::scratch().join(format!("pf-person-{tag}.json"));
    let profile = |id: &str, name: &str, seat: &str| {
        serde_json::json!({"id": id, "display_name": name,
            "os_account": {"kind": "seat", "seat": seat}})
    };
    let body = serde_json::json!({"version": 1, "profiles": [
        {"id": "4f1c3a9b0e27", "display_name": "Enrico", "os_account": {"kind": "operator"}},
        profile(PERSON, "Jane", PERSON_SEAT),
        profile("7a11ce00e002", "Sam", WINDOWS_SEAT),
        profile("7a11ce00e003", "Kid", MADE_SEAT),
    ]});
    std::fs::write(&file, body.to_string()).unwrap();
    Arc::new(crate::profiles::Profiles::load_with(Some(file), None))
}

fn password(secret: &str, stay: bool, enroll: Option<Vec<u8>>) -> v2msg::ProfileUnlock {
    let (alg, key) = match enroll {
        Some(key) => (v2msg::ENROLL_P256, key),
        None => (v2msg::ENROLL_NONE, Vec::new()),
    };
    let secret = v2msg::Zeroizing::new(secret.as_bytes().to_vec());
    v2msg::ProfileUnlock::new(v2msg::UNLOCK_PASSWORD, secret, stay, alg, key).unwrap()
}

/// A person's own Linux account opens only on the supervisor's word: each proof kind reaches it
/// from the device the QUIC handshake authenticated, under its pairing name, and only `unlocked`
/// is redirected, with `Pending` while a cold start runs. Each refusal reaches each client
/// generation as CONTRACT §5 says, and a supervisor that is down or hangs is a plain refusal.
/// The other rows are placed as before, with nothing asked.
#[test]
fn a_person_s_own_account_opens_only_on_the_supervisor_s_word() {
    let _registry = crate::session_status::tests::registry_lock();
    let _serial = SESSION_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    use crate::native::unlock::fake::{self, refused, Fake};
    use pf_seats::ipc::{ApiError, Command, CommandResult, ErrorCode, Proof};
    use punktfunk_core::quic::v2::features::FeatureSet;
    use punktfunk_core::quic::v2::registry::{FEATURE_PROFILES, FEATURE_PROFILE_UNLOCK};
    use punktfunk_core::reject::RejectReason::{ProfileLocked, SeatUnavailable};
    use std::time::Duration;
    type Answer = Result<CommandResult, ApiError>;

    let new = FeatureSet::default()
        .with(FEATURE_PROFILES)
        .with(FEATURE_PROFILE_UNLOCK);
    let old = FeatureSet::default().with(FEATURE_PROFILES);
    let unlocked = |started| -> Answer {
        Ok(CommandResult::Unlocked {
            seat: fake::person(PERSON_SEAT, 9780),
            started,
        })
    };
    let to = |port| Heard::Redirect {
        port,
        pin: "5eed".repeat(16),
    };
    let closed = |reason, sentence: &str| Heard::Closed(reason, sentence.to_string());
    let pw = |secret: &str| Sends::Unlock(password(secret, false, None));
    let p256 = [vec![4u8], vec![0x42; 64]].concat();
    let key_proof = {
        let secret = v2msg::Zeroizing::new(vec![0x30, 0x45, 0x01]);
        v2msg::ProfileUnlock::new(v2msg::UNLOCK_KEY_PROOF, secret, false, 0, Vec::new()).unwrap()
    };
    let needs = "This profile needs its account password.";
    let silent = "This computer's seat service didn't answer. Try again in a moment.";
    // (what, profile, features, sends, the supervisor's answer, its ms, what the client hears)
    let cases: Vec<(&str, &str, FeatureSet, Sends, Answer, u64, Heard)> = vec![
        (
            "a password on a cold start",
            PERSON,
            new,
            Sends::Unlock(password("hunter2 Bärbel", true, Some(p256.clone()))),
            unlocked(true),
            600,
            to(9780),
        ),
        (
            "a key proof",
            PERSON,
            new,
            Sends::Unlock(key_proof),
            unlocked(false),
            0,
            to(9780),
        ),
        (
            "a signed-in device",
            PERSON,
            new,
            Sends::Nothing,
            unlocked(false),
            0,
            to(9780),
        ),
        (
            "no proof",
            PERSON,
            new,
            Sends::Nothing,
            Err(refused("unlock_required", needs)),
            0,
            closed(ProfileLocked, ""),
        ),
        (
            "a wrong password",
            PERSON,
            new,
            pw("wrong"),
            Err(refused("credential_wrong", "That isn't jdoe's password.")),
            0,
            closed(ProfileLocked, "That isn't jdoe's password."),
        ),
        (
            "too many",
            PERSON,
            new,
            pw("wrong"),
            Err(refused("unlock_locked_out", "Try again in 15 minutes.")),
            0,
            closed(ProfileLocked, "Try again in 15 minutes."),
        ),
        (
            "a full device list",
            PERSON,
            new,
            pw("pw"),
            Err(ApiError::new(
                ErrorCode::Capacity,
                "Sign one out in the console.",
            )),
            0,
            closed(ProfileLocked, "Sign one out in the console."),
        ),
        (
            "an older client",
            PERSON,
            old,
            Sends::Nothing,
            Err(refused("unlock_required", needs)),
            0,
            closed(
                SeatUnavailable,
                "This profile needs its account password. Update Punktfunk on this device.",
            ),
        ),
        (
            "an unlock announced and not sent",
            PERSON,
            new,
            Sends::Garbled,
            unlocked(false),
            0,
            closed(
                ProfileLocked,
                "Couldn't read the password this device sent. Try again.",
            ),
        ),
        (
            "a seat that didn't start",
            PERSON,
            new,
            pw("pw"),
            Err(refused(
                "seat_dirs",
                "create /var/lib/punktfunk-seats/x: denied",
            )),
            0,
            closed(
                SeatUnavailable,
                "This profile's desktop didn't start. Try again in a moment.",
            ),
        ),
        (
            "a supervisor that is down",
            PERSON,
            new,
            pw("pw"),
            Err(ApiError::new(ErrorCode::Transport, "connect: No such file")),
            0,
            closed(SeatUnavailable, silent),
        ),
        (
            "a supervisor that hangs until the socket gives up",
            PERSON,
            new,
            pw("pw"),
            Err(ApiError::new(ErrorCode::Transport, "read: timed out")),
            1200,
            closed(SeatUnavailable, silent),
        ),
        (
            "a Windows seat",
            "7a11ce00e002",
            old,
            Sends::Nothing,
            unlocked(false),
            0,
            to(9781),
        ),
        (
            "a made seat, unlock and all",
            "7a11ce00e003",
            new,
            pw("pw"),
            unlocked(false),
            0,
            to(9782),
        ),
    ];

    // One device throughout: a redirect holds its seat for that device alone.
    let store = access_store_path("person");
    let _ = std::fs::remove_file(&store);
    let np = Arc::new(NativePairing::load_with(Some(store.clone()), None, false).unwrap());
    let (cert, key) = endpoint::generate_identity().unwrap();
    let fp = hex::encode(endpoint::fingerprint_of_pem(&cert).unwrap());
    np.add_with_access("Jane's phone", &fp, None).unwrap();
    let host = spawn_profile_host(19797, cases.len() as u32, np, person_profiles("cases"));
    std::thread::sleep(Duration::from_millis(500));
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    for (what, profile, features, sends, answer, takes, heard) in cases {
        let kind = match &sends {
            Sends::Unlock(u) if u.kind == v2msg::UNLOCK_PASSWORD => "password",
            Sends::Unlock(_) => "key",
            _ => "none",
        };
        let garbled = matches!(sends, Sends::Garbled);
        let mut sup = Fake::new(person_ledger(), move |c| match c {
            Command::Unlock { .. } => answer.clone(),
            _ => Ok(CommandResult::Left),
        });
        sup.unlock_takes = Duration::from_millis(takes);
        let sup = Arc::new(sup);
        let _armed = fake::arm(&fp, sup.clone());
        let dialled = dial_person(19797, (&cert, &key), profile, features, sends);
        let (pendings, got) = rt.block_on(dialled);
        assert_eq!(got, heard, "{what}");
        if takes > 0 {
            assert!(pendings >= 1, "{what}: Pending while the supervisor works");
        }
        let asked = sup.asked();
        let unlocks: Vec<_> = asked
            .iter()
            .filter(|c| matches!(c, Command::Unlock { .. }))
            .collect();
        if profile != PERSON || garbled {
            assert!(unlocks.is_empty(), "{what}: nothing asked");
            continue;
        }
        let [Command::Unlock { id, device, proof }] = unlocks[..] else {
            panic!("{what}: one unlock asked, got {asked:?}");
        };
        assert_eq!(id.as_str(), PERSON_SEAT, "{what}");
        assert_eq!(device.fingerprint, fp, "{what}: the certificate's own");
        assert_eq!(device.name, "Jane's phone", "{what}: its pairing name");
        assert_eq!(proof.kind(), kind, "{what}");
        match proof {
            Proof::Password {
                password,
                stay_signed_in: true,
                enroll: Some(key),
            } => {
                assert_eq!(password.expose(), "hunter2 Bärbel");
                assert_eq!(key.public, hex::encode(&p256));
            }
            Proof::Key { signature } => assert_eq!(signature, "304501"),
            _ => {}
        }
    }
    host.join().unwrap().unwrap();
    let _ = std::fs::remove_file(&store);
}

/// A wrong password leaves no trace of itself in the host's log at any level, in its errors or
/// in what the client reads: only that an unlock was asked and how it ended.
#[test]
fn a_wrong_password_never_reaches_a_log_or_an_error() {
    let _registry = crate::session_status::tests::registry_lock();
    let _serial = SESSION_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    use crate::native::unlock::fake::{self, refused, Fake};
    use punktfunk_core::quic::v2::features::FeatureSet;
    use punktfunk_core::quic::v2::registry::{FEATURE_PROFILES, FEATURE_PROFILE_UNLOCK};

    #[derive(Clone, Default)]
    struct Lines(Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for Lines {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let lines = Lines::default();
    let writer = lines.clone();
    let logs = tracing::Dispatch::new(
        tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish(),
    );
    let secret = "Pf-n0t-H1s-pa55word";
    let store = access_store_path("person-logs");
    let _ = std::fs::remove_file(&store);
    let np = Arc::new(NativePairing::load_with(Some(store.clone()), None, false).unwrap());
    let (cert, key) = endpoint::generate_identity().unwrap();
    let fp = hex::encode(endpoint::fingerprint_of_pem(&cert).unwrap());
    np.add_with_access("Jane's phone", &fp, None).unwrap();
    let sup = Arc::new(Fake::new(person_ledger(), |_| {
        Err(refused("credential_wrong", "That isn't jdoe's password."))
    }));
    let _armed = fake::arm(&fp, sup.clone());
    let host = spawn_logged_host(19798, 1, np, person_profiles("logs"), Some(logs));
    std::thread::sleep(std::time::Duration::from_millis(500));
    let features = FeatureSet::default()
        .with(FEATURE_PROFILES)
        .with(FEATURE_PROFILE_UNLOCK);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let unlock = Sends::Unlock(password(secret, false, None));
    let (_, heard) = rt.block_on(dial_person(19798, (&cert, &key), PERSON, features, unlock));
    host.join().unwrap().unwrap();
    let _ = std::fs::remove_file(&store);

    let said = String::from_utf8(lines.0.lock().unwrap().clone()).unwrap();
    assert!(
        said.contains("profile unlock answered"),
        "the log was captured: {said}"
    );
    assert!(
        said.contains("credential_wrong"),
        "the outcome code is logged"
    );
    for trace in [said, format!("{heard:?}"), format!("{:?}", sup.asked())] {
        assert!(!trace.contains(secret), "{trace}");
        assert!(!trace.contains(&hex::encode(secret)), "{trace}");
    }
    assert_eq!(
        heard,
        Heard::Closed(
            punktfunk_core::reject::RejectReason::ProfileLocked,
            "That isn't jdoe's password.".into()
        )
    );
}

const OWNER_PROFILE: &str = "4f1c3a9b0e27";
const KID_PROFILE: &str = "c1d000000001";

/// A device's dial: the profile it names, the features it offers, what it sends after its hello.
type Dial<'a> = (
    &'a str,
    punktfunk_core::quic::v2::features::FeatureSet,
    Sends,
);

/// The features of a client that follows redirects, and with `prompts` also asks for an unlock.
fn offers(prompts: bool) -> punktfunk_core::quic::v2::features::FeatureSet {
    use punktfunk_core::quic::v2::registry::{FEATURE_PROFILES, FEATURE_PROFILE_UNLOCK};
    let features = punktfunk_core::quic::v2::features::FeatureSet::default().with(FEATURE_PROFILES);
    match prompts {
        true => features.with(FEATURE_PROFILE_UNLOCK),
        false => features,
    }
}

/// A box of its own on `port`: the owner on the box's own session, and [`KID_PROFILE`] on a made
/// seat on `port + 1000` that no other test's redirect holds. `locks`, given the device's
/// fingerprint, is the profile lock file, `null` for none. One paired device makes `dials` in
/// turn; what it hears each time, and where the lock file is.
fn dial_box(
    port: u16,
    locks: impl FnOnce(&str) -> serde_json::Value,
    dials: Vec<Dial<'_>>,
) -> (Vec<Heard>, std::path::PathBuf) {
    dial_host(port, false, locks, dials)
}

/// [`dial_box`], on the kid's seat host when `seat_host`: it reads the box's files read only, as
/// a Windows seat host reads the box's config dir.
fn dial_host(
    port: u16,
    seat_host: bool,
    locks: impl FnOnce(&str) -> serde_json::Value,
    dials: Vec<Dial<'_>>,
) -> (Vec<Heard>, std::path::PathBuf) {
    use crate::native::unlock::fake::{self, Fake};
    let dir = crate::test_support::scratch();
    let seat_id = format!("{port:032x}");
    let mut seat = fake::person(&seat_id, port + 1000);
    seat.adopted_uid = None;
    let file = serde_json::json!({"version": 1, "profiles": [
        {"id": OWNER_PROFILE, "display_name": "Enrico", "os_account": {"kind": "operator"}},
        {"id": KID_PROFILE, "display_name": "Kid", "os_account": {"kind": "seat", "seat": seat_id}},
    ]});
    std::fs::write(dir.join("profiles.json"), file.to_string()).unwrap();
    let np = NativePairing::load_with(Some(dir.join("punktfunk1-paired.json")), None, false);
    let np = Arc::new(np.unwrap());
    let (cert, key) = endpoint::generate_identity().unwrap();
    let fp = hex::encode(endpoint::fingerprint_of_pem(&cert).unwrap());
    np.add_with_access("Kid's tablet", &fp, None).unwrap();
    let lock_file = dir.join(pf_seats::locks::FILE);
    let locks = locks(&fp);
    if !locks.is_null() {
        std::fs::write(&lock_file, locks.to_string()).unwrap();
    }
    let path = dir.join("profiles.json");
    let profiles = match seat_host {
        true => crate::profiles::Profiles::load_box(path, Some(seat_id)),
        false => crate::profiles::Profiles::load_with(Some(path), None),
    };
    let sup = Fake::new(vec![seat], |_| Ok(pf_seats::ipc::CommandResult::Left));
    let _armed = fake::arm(&fp, Arc::new(sup));
    let host = spawn_profile_host(port, dials.len() as u32, np, Arc::new(profiles));
    std::thread::sleep(std::time::Duration::from_millis(500));
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let heard = dials
        .into_iter()
        .map(|(profile, features, sends)| {
            let dialled = dial_person(port, (&cert, &key), profile, features, sends);
            rt.block_on(dialled).1
        })
        .collect();
    host.join().unwrap().unwrap();
    (heard, lock_file)
}

/// The kid's seat as a redirect from `port` names it.
fn kid_seat(port: u16) -> Heard {
    Heard::Redirect {
        port: port + 1000,
        pin: "5eed".repeat(16),
    }
}

/// An open profile opens as it always has: the box's own session plays here and a seat's profile
/// goes to its seat, with nothing asked of the device.
#[test]
fn an_open_profile_opens_on_every_session_path() {
    let _registry = crate::session_status::tests::registry_lock();
    let _serial = SESSION_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let dials = vec![
        (OWNER_PROFILE, offers(true), Sends::Nothing),
        (KID_PROFILE, offers(false), Sends::Nothing),
    ];
    let (heard, _) = dial_box(19801, |_| serde_json::Value::Null, dials);
    assert_eq!(heard, [Heard::Played, kid_seat(19801)]);
}

/// `fp` on a profile's list, signed in until signed out.
fn listed(fp: &str) -> serde_json::Value {
    serde_json::json!({"fingerprint": fp, "name": "Kid's tablet", "stay": true,
        "added_unix": 1, "last_unix": 1})
}

/// The refusal a device hears for a locked profile it is not on the list of.
fn not_allowed() -> Heard {
    Heard::Closed(
        punktfunk_core::reject::RejectReason::SeatUnavailable,
        "This device can't open this profile. Ask the host's owner to add it.".into(),
    )
}

/// A locked profile opens for the devices on its list alone, on both session paths: the box's
/// own session, played here, and a seat's profile, refused before its redirect. A lock entry
/// that isn't locked is an open profile.
#[test]
fn a_locked_profile_opens_only_for_its_devices() {
    let _registry = crate::session_status::tests::registry_lock();
    let _serial = SESSION_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let dials = || {
        vec![
            (OWNER_PROFILE, offers(true), Sends::Nothing),
            (KID_PROFILE, offers(false), Sends::Nothing),
        ]
    };
    let other = |_: &str| {
        let lock = serde_json::json!({"locked": true, "devices": [listed(&"ab".repeat(32))]});
        serde_json::json!({"version": 1, "profiles": {OWNER_PROFILE: lock, KID_PROFILE: lock}})
    };
    let (heard, _) = dial_box(19802, other, dials());
    assert_eq!(heard, [not_allowed(), not_allowed()]);

    let mine = |fp: &str| {
        let lock = serde_json::json!({"locked": true, "devices": [listed(fp)]});
        let open = serde_json::json!({"locked": false});
        serde_json::json!({"version": 1, "profiles": {OWNER_PROFILE: lock, KID_PROFILE: open}})
    };
    let (heard, _) = dial_box(19803, mine, dials());
    assert_eq!(heard, [Heard::Played, kid_seat(19803)]);
}

/// A seat host reads the box's own lock file, as a Windows seat host reads the box's config dir:
/// a device not on the list is refused there and never asked for the PIN only the box takes; a
/// device on it goes on.
#[test]
fn a_seat_host_checks_the_box_s_lock_file() {
    let _registry = crate::session_status::tests::registry_lock();
    let _serial = SESSION_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let pin = serde_json::to_value(pf_seats::locks::PinHash::new("2468").unwrap()).unwrap();
    let other = |_: &str| {
        let lock = serde_json::json!({"locked": true, "pin": pin,
            "devices": [listed(&"ab".repeat(32))]});
        serde_json::json!({"version": 1, "profiles": {KID_PROFILE: lock}})
    };
    let dials = vec![(KID_PROFILE, offers(true), Sends::Nothing)];
    let (heard, _) = dial_host(19805, true, other, dials);
    assert_eq!(heard, [not_allowed()]);

    let mine = |fp: &str| {
        let lock = serde_json::json!({"locked": true, "devices": [listed(fp)]});
        serde_json::json!({"version": 1, "profiles": {KID_PROFILE: lock}})
    };
    let dials = vec![(KID_PROFILE, offers(true), Sends::Nothing)];
    let (heard, _) = dial_host(19806, true, mine, dials);
    assert_eq!(heard, [kid_seat(19806)]);
}

/// A session on a locked profile ends once its device is signed out, with the sentence that says
/// so.
#[test]
fn signing_a_device_out_ends_its_session() {
    let _registry = crate::session_status::tests::registry_lock();
    let _serial = SESSION_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let dir = crate::test_support::scratch();
    let owner = serde_json::json!({"id": OWNER_PROFILE, "display_name": "Enrico",
        "os_account": {"kind": "operator"}});
    let file = serde_json::json!({"version": 1, "profiles": [owner]});
    std::fs::write(dir.join("profiles.json"), file.to_string()).unwrap();
    let (cert, key) = endpoint::generate_identity().unwrap();
    let fp = hex::encode(endpoint::fingerprint_of_pem(&cert).unwrap());
    let lock = serde_json::json!({"locked": true, "devices": [listed(&fp)]});
    let locks = serde_json::json!({"version": 1, "profiles": {OWNER_PROFILE: lock}});
    std::fs::write(dir.join(pf_seats::locks::FILE), locks.to_string()).unwrap();
    let profiles = Arc::new(crate::profiles::Profiles::load_with(
        Some(dir.join("profiles.json")),
        None,
    ));
    let np = NativePairing::load_with(Some(dir.join("punktfunk1-paired.json")), None, false);
    let np = Arc::new(np.unwrap());
    np.add_with_access("Kid's tablet", &fp, None).unwrap();
    let host = spawn_profile_host(19804, 1, np, profiles.clone());
    std::thread::sleep(std::time::Duration::from_millis(500));
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let (code, reason) = rt.block_on(async {
        let (conn, _send, _recv, _welcome, _media) = raw_session(19804, (&cert, &key)).await;
        let signed_out = profiles.locks().edit(OWNER_PROFILE, |lock, _| {
            lock.list.sign_out(None);
        });
        signed_out.unwrap();
        let closed = tokio::time::timeout(std::time::Duration::from_secs(10), conn.closed());
        match closed.await.expect("the host ends the session") {
            quinn::ConnectionError::ApplicationClosed(ac) => (
                u64::from(ac.error_code),
                String::from_utf8(ac.reason.to_vec()).unwrap(),
            ),
            other => panic!("expected the host's close, got {other:?}"),
        }
    });
    host.join().unwrap().unwrap();
    let unavailable = punktfunk_core::reject::RejectReason::SeatUnavailable.close_code();
    assert_eq!(code, u64::from(unavailable));
    assert_eq!(reason, "This device was signed out of this profile.");
}
