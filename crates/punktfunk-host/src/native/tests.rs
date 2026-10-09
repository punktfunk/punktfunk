use super::*;
use punktfunk_core::quic::v2::msg::V2Message;

/// The knob is the only way in, and an unpaired session never gets a seat home.
#[cfg(target_os = "linux")]
#[test]
fn only_a_seat_profile_with_the_knob_on_gets_a_seat_home() {
    let seat = seat_home_for(Some("9a3f1c2b7e40"), true).expect("a seat profile has a home");
    assert!(seat.ends_with("seats/9a3f1c2b7e40"), "{}", seat.display());
    assert_eq!(seat_home_for(Some("9a3f1c2b7e40"), false), None, "knob off");
    assert_eq!(
        seat_home_for(None, true),
        None,
        "the owner keeps the box's Steam"
    );
}

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
    assert!(first.is_first(&base) && !second.is_first(&base));
    let (a, b) = (
        session_isolation(&first.0, Some("5e1f00d1e2a7")),
        session_isolation(&second.0, None),
    );
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

    let said = setup_failed_sentence(&buried).expect("a pin miss has words for the user");
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
    let said = setup_failed_sentence(&asleep).expect("a dark display has words for the user");
    assert!(said.starts_with("The host's screen is asleep"), "{said}");
    assert!(said.len() <= punktfunk_core::quic::REFUSED_REASON_MAX);

    // Anything else keeps the client's own wording rather than leaking a chain.
    let other = anyhow::anyhow!("open NVENC: device busy").context("build the session pipeline");
    assert_eq!(setup_failed_sentence(&other), None);
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
    while got < count {
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

/// Clipboard over a synthetic session: host advertises the cap, acks enable with
/// `BACKEND_UNAVAILABLE` (no compositor), declines a fetch. Live-backend paths are
/// not covered here. `design/clipboard-and-file-transfer.md`.
#[test]
fn clipboard_control_and_fetch_decline_over_session() {
    let _registry = crate::session_status::tests::registry_lock();
    let _serial = SESSION_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    use punktfunk_core::client::{ConnectParams, NativeClient};
    use punktfunk_core::clipboard::ClipEventCore;
    use punktfunk_core::quic::{
        CLIP_FILE_INDEX_NONE, CLIP_FLAG_FILES, CLIP_POLICY_FILES, HOST_CAP_CLIPBOARD,
    };

    // Restore the env even on panic so a leaked var cannot reach the next session test.
    struct EnvGuard(&'static str);
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // SAFETY: dropped while SESSION_TEST_LOCK is held; only the session path reads this.
            unsafe { std::env::remove_var(self.0) };
            pf_host_config::reload();
        }
    }
    let _env = EnvGuard("PUNKTFUNK_CLIPBOARD");
    // Operator policy on. Serialized on SESSION_TEST_LOCK; only the session path reads this.
    // SAFETY: writers serialized; only this session path reads the variable.
    unsafe { std::env::set_var("PUNKTFUNK_CLIPBOARD", "1") };
    pf_host_config::reload();

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

fn test_paired_path() -> std::path::PathBuf {
    std::env::temp_dir().join(format!("punktfunk-paired-test-{}.json", std::process::id()))
}

/// Unpaired knock is parked; approve while waiting admits the same connection, no reconnect.
#[test]
fn delegated_approval_admits_after_knock() {
    let _registry = crate::session_status::tests::registry_lock();
    let _serial = SESSION_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    use punktfunk_core::client::{ConnectParams, NativeClient};
    use punktfunk_core::quic::endpoint;

    let store = std::env::temp_dir().join(format!("pf-approval-test-{}.json", std::process::id()));
    let _ = std::fs::remove_file(&store);
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
        np_approve
            .approve_pending(pend.id, Some("Approved Device"), None)
            .unwrap()
            .paired()
            .expect("pending id must approve");
    });

    // One connect that parks until approved, then streams. Timeout covers park + approver poll.
    // No Hello name: assert the fingerprint-derived label. TOFU: approval, not a PIN,
    // authorizes this client.
    let client = NativeClient::connect(ConnectParams {
        identity: Some((cert, key)),
        ..ConnectParams::new("127.0.0.1", 19779, mode, std::time::Duration::from_secs(15))
    })
    .expect("approved mid-park → session admitted with no reconnect");
    approver.join().unwrap();
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

/// Right PIN pairs; paired identity gets a session; anonymous does not.
#[test]
fn pairing_ceremony_and_gate() {
    let _registry = crate::session_status::tests::registry_lock();
    let _serial = SESSION_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    use punktfunk_core::client::{ConnectParams, NativeClient};
    use punktfunk_core::quic::endpoint;

    let host = std::thread::spawn(|| {
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
            paired_store: Some(test_paired_path()),
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
    assert!(test_paired_path().exists());

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
    let _ = std::fs::remove_file(test_paired_path());

    host.join().unwrap().unwrap();
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
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
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
    std::env::temp_dir().join(format!("pf-access-{tag}-{}.json", std::process::id()))
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
    use punktfunk_core::quic::v2::hello::{ClientHello, Ready, ServerHello};
    use punktfunk_core::quic::v2::{io as v2io, msg, registry};
    let (ep, _observed) = endpoint::client_shared(None, Some(identity), &[registry::ALPN]);
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
    };
    let hello = ClientHello {
        hello,
        client_label: None,
        abr_features: 0,
        preset: None,
        link: Default::default(),
        probe_only: false,
        pyrowave_bpp_x100: 0,
        resume: None,
        suites: Vec::new(),
        features: Default::default(),
        profile: None,
    };
    v2io::send(&mut send, &hello).await.expect("ClientHello");
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
    np.add_with_access(
        "Evening Guest",
        &fp_hex,
        Some(crate::native_pairing::Access {
            grants: GRANT_ALL,
            expires_unix: Some(crate::clock::unix_secs() + 2),
            until_disconnect: false,
        }),
    )
    .unwrap();
    let host = spawn_access_host(19782, 1, np.clone());
    std::thread::sleep(std::time::Duration::from_millis(500));

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let (conn, _send, _recv, welcome, _media) =
            raw_session(19782, (cert.as_str(), key.as_str())).await;
        assert_eq!(welcome.grants, GRANT_ALL, "the Welcome advertises the mask");
        assert!(
            (1..=2).contains(&welcome.expires_in_secs),
            "a 2 s grant must advertise 1–2 remaining secs, got {}",
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
        Err(punktfunk_core::PunktfunkError::Rejected(r)) => assert_eq!(
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
    let file =
        std::env::temp_dir().join(format!("pf-handshake-profiles-{}.json", std::process::id()));
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
        Err(punktfunk_core::PunktfunkError::Rejected(r)) => {
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
