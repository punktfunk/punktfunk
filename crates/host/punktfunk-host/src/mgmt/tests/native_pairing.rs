//! Native pairing: arm, approve and deny, access masks and the trust store.

use super::*;

#[tokio::test]
async fn native_pairing_arm_show_and_unpair() {
    let np = Arc::new(
        crate::native_pairing::NativePairing::load_with(
            Some(std::env::temp_dir().join(format!("pf-mgmt-np-{}.json", std::process::id()))),
            None,
            false,
        )
        .unwrap(),
    );
    let app = test_app_native(test_state(), np.clone());

    let (s, b) = send(&app, get_req("/api/v1/native/pair")).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(b["enabled"], true);
    assert_eq!(b["armed"], false);
    assert!(b["pin"].is_null());

    let (s, b) = send(
        &app,
        json_req(
            Method::POST,
            "/api/v1/native/pair/arm",
            serde_json::json!({"ttl_secs": 60}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(b["armed"], true);
    let pin = b["pin"].as_str().unwrap().to_string();
    assert_eq!(pin.len(), 4);
    let (_, b) = send(&app, get_req("/api/v1/native/pair")).await;
    assert_eq!(b["pin"], pin);
    assert!(b["expires_in_secs"].as_u64().unwrap() <= 60);

    // QUIC reads the same live PIN.
    assert_eq!(np.current_pin().as_deref(), Some(pin.as_str()));

    np.add("Test Device", "abc123").unwrap();
    let (s, b) = send(&app, get_req("/api/v1/native/clients")).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(b[0]["name"], "Test Device");
    assert_eq!(b[0]["fingerprint"], "abc123");
    let del = axum::http::Request::delete("/api/v1/native/clients/ABC123")
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&app, del).await.0, StatusCode::NO_CONTENT);
    let missing = axum::http::Request::delete("/api/v1/native/clients/abc123")
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&app, missing).await.0, StatusCode::NOT_FOUND);

    let del = axum::http::Request::delete("/api/v1/native/pair")
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&app, del).await.0, StatusCode::NO_CONTENT);
    let (_, b) = send(&app, get_req("/api/v1/native/pair")).await;
    assert_eq!(b["armed"], false);
}

#[tokio::test]
async fn native_unpair_all_empties_the_trust_store() {
    let np = Arc::new(
        crate::native_pairing::NativePairing::load_with(
            Some(std::env::temp_dir().join(format!("pf-mgmt-np-all-{}.json", std::process::id()))),
            None,
            false,
        )
        .unwrap(),
    );
    let app = test_app_native(test_state(), np.clone());

    np.add("Living room TV", "aa11").unwrap();
    np.add("Studio Deck", "bb22").unwrap();
    assert_eq!(np.list().len(), 2);

    let del_all = || {
        axum::http::Request::delete("/api/v1/native/clients")
            .body(Body::empty())
            .unwrap()
    };
    let (status, body) = send(&app, del_all()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["unpaired"], 2);

    // One persisted write, not two.
    let (_, body) = send(&app, get_req("/api/v1/native/clients")).await;
    assert_eq!(body, serde_json::json!([]));
    assert!(np.list().is_empty());
    assert!(!np.is_paired("aa11") && !np.is_paired("bb22"));

    // Idempotent; the single delete 404s a missing fingerprint.
    let (status, body) = send(&app, del_all()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["unpaired"], 0);
}

/// No native plane → 503, matching every other `/native/*`. Not a 200 that claims an unpair.
#[tokio::test]
async fn native_unpair_all_without_a_native_host_is_unavailable() {
    let app = test_app(test_state(), None);
    let req = axum::http::Request::delete("/api/v1/native/clients")
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&app, req).await.0, StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn pending_devices_approve_and_deny() {
    let np = Arc::new(
        crate::native_pairing::NativePairing::load_with(
            Some(std::env::temp_dir().join(format!("pf-mgmt-pending-{}.json", std::process::id()))),
            None,
            false,
        )
        .unwrap(),
    );
    let app = test_app_native(test_state(), np.clone());

    let (s, b) = send(&app, get_req("/api/v1/native/pending")).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(b.as_array().unwrap().len(), 0);

    np.note_pending("Enrico's MacBook", "aa11", Some(LAN_KNOCK), None);
    np.note_pending("device bb22cc33", "bb22", Some(LAN_KNOCK), None);
    let (_, b) = send(&app, get_req("/api/v1/native/pending")).await;
    assert_eq!(b.as_array().unwrap().len(), 2);
    assert_eq!(b[0]["name"], "Enrico's MacBook");
    let approve_id = b[0]["id"].as_u64().unwrap();
    let deny_id = b[1]["id"].as_u64().unwrap();

    let (s, b) = send(
        &app,
        json_req(
            Method::POST,
            &format!("/api/v1/native/pending/{approve_id}/approve"),
            serde_json::json!({"name": "Office MacBook"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(b["name"], "Office MacBook");
    assert_eq!(b["fingerprint"], "aa11");
    assert!(np.is_paired("AA11"), "approval pins the fingerprint");

    let deny = json_req(
        Method::POST,
        &format!("/api/v1/native/pending/{deny_id}/deny"),
        serde_json::json!({}),
    );
    assert_eq!(send(&app, deny).await.0, StatusCode::NO_CONTENT);
    assert!(!np.is_paired("bb22"));
    let (s, _) = send(
        &app,
        json_req(
            Method::POST,
            &format!("/api/v1/native/pending/{deny_id}/deny"),
            serde_json::json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    // Empty `{}` keeps the device's own name; a stale id 404s.
    let (_, b) = send(&app, get_req("/api/v1/native/pending")).await;
    assert_eq!(b.as_array().unwrap().len(), 0);
    let (s, _) = send(
        &app,
        json_req(
            Method::POST,
            "/api/v1/native/pending/123/approve",
            serde_json::json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

/// Host wall clock, unix seconds — relative-in / absolute-stored conversion.
fn wall_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// Omitted PATCH halves keep their current value; `clear_expiry` makes access permanent.
/// The live-session watch must fire on the same edit.
#[tokio::test]
async fn patch_native_access_reflects_in_list_and_fires_watch() {
    use punktfunk_core::quic::{GRANT_GAMEPAD, GRANT_POINTER};
    let np = Arc::new(
        crate::native_pairing::NativePairing::load_with(
            Some(std::env::temp_dir().join(format!("pf-mgmt-patch-{}.json", std::process::id()))),
            None,
            false,
        )
        .unwrap(),
    );
    let app = test_app_native(test_state(), np.clone());

    np.add("Living Room TV", "aa11").unwrap();
    // What a live session holds at admission; the edit must reach it within one event.
    let mut rx = np.subscribe("aa11");

    // 7200 s = 2 hours. Fingerprint path is case-insensitive, like DELETE.
    let now = wall_now();
    let (s, b) = send(
        &app,
        json_req(
            Method::PATCH,
            "/api/v1/native/clients/AA11",
            serde_json::json!({"grants": GRANT_GAMEPAD, "expires_in_secs": 7200}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(b["grants"], GRANT_GAMEPAD);
    assert_eq!(b["access_level"], "controller");
    let deadline = b["expires_unix"].as_i64().unwrap();
    assert!(
        (now + 7200..=now + 7202).contains(&deadline),
        "relative expiry stored as an absolute deadline: {deadline}"
    );
    assert!(b["granted_unix"].as_i64().unwrap() >= now, "grant stamped");

    let (_, list) = send(&app, get_req("/api/v1/native/clients")).await;
    assert_eq!(list[0]["grants"], GRANT_GAMEPAD);
    assert_eq!(list[0]["access_level"], "controller");
    assert_eq!(list[0]["expires_unix"].as_i64().unwrap(), deadline);

    assert!(rx.has_changed().unwrap(), "the access watch must fire");
    {
        let state = rx.borrow_and_update();
        assert_eq!(state.grants, GRANT_GAMEPAD);
        assert_eq!(state.deadline_unix, Some(deadline));
        assert!(!state.revoked);
    }

    let (s, b) = send(
        &app,
        json_req(
            Method::PATCH,
            "/api/v1/native/clients/aa11",
            serde_json::json!({"expires_in_secs": 60}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(b["grants"], GRANT_GAMEPAD, "omitted grants keep current");
    let short_deadline = b["expires_unix"].as_i64().unwrap();
    assert!(short_deadline < deadline, "the expiry did change");

    // Omitted expiry keeps the stored deadline exactly, not re-derived.
    let (s, b) = send(
        &app,
        json_req(
            Method::PATCH,
            "/api/v1/native/clients/aa11",
            serde_json::json!({"grants": 0}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(b["access_level"], "view");
    assert_eq!(
        b["expires_unix"].as_i64().unwrap(),
        short_deadline,
        "omitted expiry keeps current"
    );

    let (s, b) = send(
        &app,
        json_req(
            Method::PATCH,
            "/api/v1/native/clients/aa11",
            serde_json::json!({"clear_expiry": true}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert!(b["expires_unix"].is_null(), "clear_expiry = permanent");
    assert_eq!(b["access_level"], "view");

    let (_, b) = send(
        &app,
        json_req(
            Method::PATCH,
            "/api/v1/native/clients/aa11",
            serde_json::json!({"grants": GRANT_GAMEPAD | GRANT_POINTER}),
        ),
    )
    .await;
    assert_eq!(b["access_level"], "custom");
}

/// Reserved bits and expiry-field conflict 400 without writing. Unknown fingerprint is 404
/// (PATCH is not a pairing path). No native plane is 503.
#[tokio::test]
async fn patch_native_access_validates_and_404s() {
    use punktfunk_core::quic::{GRANT_ALL, GRANT_GAMEPAD};
    let np = Arc::new(
        crate::native_pairing::NativePairing::load_with(
            Some(
                std::env::temp_dir().join(format!("pf-mgmt-patch-val-{}.json", std::process::id())),
            ),
            None,
            false,
        )
        .unwrap(),
    );
    let app = test_app_native(test_state(), np.clone());
    np.add("Deck", "bb22").unwrap();

    let (s, b) = send(
        &app,
        json_req(
            Method::PATCH,
            "/api/v1/native/clients/bb22",
            serde_json::json!({"grants": GRANT_ALL | (1u32 << 30)}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(b["error"].as_str().unwrap().contains("reserved"));
    assert_eq!(np.list()[0].grants, None, "a 400 writes nothing");

    let (s, b) = send(
        &app,
        json_req(
            Method::PATCH,
            "/api/v1/native/clients/bb22",
            serde_json::json!({"expires_in_secs": 60, "clear_expiry": true}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(b["error"].as_str().unwrap().contains("clear_expiry"));

    let (s, _) = send(
        &app,
        json_req(
            Method::PATCH,
            "/api/v1/native/clients/nope99",
            serde_json::json!({"grants": GRANT_GAMEPAD}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert!(!np.is_paired("nope99"), "PATCH must never pair a device");

    let plain = test_app(test_state(), None);
    let (s, _) = send(
        &plain,
        json_req(
            Method::PATCH,
            "/api/v1/native/clients/bb22",
            serde_json::json!({"grants": GRANT_GAMEPAD}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
}

/// `until_disconnect` replaces nothing on its own. Honouring it alone would hand a re-pairing
/// device full permanent control, because `Access` replaces the whole record.
#[tokio::test]
async fn until_disconnect_alone_is_refused_rather_than_widening_access() {
    let np = Arc::new(
        crate::native_pairing::NativePairing::load_with(
            Some(
                std::env::temp_dir().join(format!("pf-mgmt-lone-udc-{}.json", std::process::id())),
            ),
            None,
            false,
        )
        .unwrap(),
    );
    let app = test_app_native(test_state(), np.clone());
    np.note_pending("Guest Phone", "cc33", Some(LAN_KNOCK), None);
    let id = np.pending()[0].id;

    let (s, _) = send(
        &app,
        json_req(
            Method::POST,
            &format!("/api/v1/native/pending/{id}/approve"),
            serde_json::json!({"until_disconnect": true}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(!np.is_paired("cc33"), "a 400 must not consume the knock");
    assert_eq!(np.pending().len(), 1);

    // Arming refuses it on the same terms, and leaves no window open.
    let (s, _) = send(
        &app,
        json_req(
            Method::POST,
            "/api/v1/native/pair/arm",
            serde_json::json!({"until_disconnect": true}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(!np.status().armed, "a refused arm leaves no window");

    // With an access level beside it, it lands.
    let (s, b) = send(
        &app,
        json_req(
            Method::POST,
            &format!("/api/v1/native/pending/{id}/approve"),
            serde_json::json!({"grants": 1, "until_disconnect": true}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(b["until_disconnect"], true);
    assert_eq!(b["grants"], 1, "grants stay what the operator chose");
}

/// The pending list says where a knock came from, and the endpoint refuses to admit one from
/// the internet — the console can hide its Approve button, but the rule is enforced here.
#[tokio::test]
async fn a_wan_knock_is_listed_as_wan_and_refused_by_approve() {
    let np = Arc::new(
        crate::native_pairing::NativePairing::load_with(
            Some(
                std::env::temp_dir().join(format!("pf-mgmt-wan-knock-{}.json", std::process::id())),
            ),
            None,
            false,
        )
        .unwrap(),
    );
    let app = test_app_native(test_state(), np.clone());
    let wan = std::net::IpAddr::V4(std::net::Ipv4Addr::new(203, 0, 113, 5));
    np.note_pending("Friend's Deck", "dd88", Some(wan), None);
    np.note_pending("Living Room", "ee99", Some(LAN_KNOCK), None);

    let (_, b) = send(&app, get_req("/api/v1/native/pending")).await;
    let rows = b.as_array().unwrap();
    let wan_row = rows.iter().find(|r| r["fingerprint"] == "dd88").unwrap();
    let lan_row = rows.iter().find(|r| r["fingerprint"] == "ee99").unwrap();
    assert_eq!(wan_row["source"], "wan");
    assert_eq!(lan_row["source"], "lan");

    let (s, _) = send(
        &app,
        json_req(
            Method::POST,
            &format!(
                "/api/v1/native/pending/{}/approve",
                wan_row["id"].as_u64().unwrap()
            ),
            serde_json::json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "a WAN knock needs a bound PIN");
    assert!(!np.is_paired("dd88"));
    assert_eq!(
        np.pending().len(),
        2,
        "the refusal leaves the knock waiting"
    );

    // The LAN one goes through, so the refusal is about the source and nothing else.
    let (s, _) = send(
        &app,
        json_req(
            Method::POST,
            &format!(
                "/api/v1/native/pending/{}/approve",
                lan_row["id"].as_u64().unwrap()
            ),
            serde_json::json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert!(np.is_paired("ee99"));
}

/// Approve pins the chosen mask. Reserved bits 400 without consuming the pending entry.
/// A later re-knock surfaces the stored access.
#[tokio::test]
async fn approve_with_access_pins_the_chosen_mask() {
    use punktfunk_core::quic::{GRANT_ALL, GRANT_GAMEPAD};
    let np = Arc::new(
        crate::native_pairing::NativePairing::load_with(
            Some(
                std::env::temp_dir()
                    .join(format!("pf-mgmt-approve-acc-{}.json", std::process::id())),
            ),
            None,
            false,
        )
        .unwrap(),
    );
    let app = test_app_native(test_state(), np.clone());

    np.note_pending("Guest Phone", "cc33", Some(LAN_KNOCK), None);
    let (_, pend) = send(&app, get_req("/api/v1/native/pending")).await;
    assert!(pend[0]["grants"].is_null());
    assert!(pend[0]["access_level"].is_null());
    let id = pend[0]["id"].as_u64().unwrap();

    let (s, _) = send(
        &app,
        json_req(
            Method::POST,
            &format!("/api/v1/native/pending/{id}/approve"),
            serde_json::json!({"grants": GRANT_ALL | (1u32 << 31)}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(
        np.pending_contains("cc33"),
        "a 400 must not consume the knock"
    );
    assert!(!np.is_paired("cc33"));

    // 14400 s = 4 hours.
    let now = wall_now();
    let (s, b) = send(
        &app,
        json_req(Method::POST,
            &format!("/api/v1/native/pending/{id}/approve"),
            serde_json::json!({"name": "Guest Phone", "grants": GRANT_GAMEPAD, "expires_in_secs": 14400}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(b["name"], "Guest Phone");
    assert_eq!(b["grants"], GRANT_GAMEPAD);
    assert_eq!(b["access_level"], "controller");
    let deadline = b["expires_unix"].as_i64().unwrap();
    assert!((now + 14400..=now + 14402).contains(&deadline));
    assert!(b["granted_unix"].as_i64().unwrap() >= now, "grant stamped");
    assert_eq!(np.effective("cc33", now), Some(GRANT_GAMEPAD));

    // Re-knock surfaces the stored access for the approve dialog.
    np.note_pending("Guest Phone", "cc33", Some(LAN_KNOCK), None);
    let (_, pend) = send(&app, get_req("/api/v1/native/pending")).await;
    assert_eq!(pend[0]["grants"], GRANT_GAMEPAD);
    assert_eq!(pend[0]["access_level"], "controller");
    assert_eq!(pend[0]["expires_unix"].as_i64().unwrap(), deadline);
}

/// Armed window carries the choice with relative expiry already absolute. Reserved bits
/// 400 before a window opens.
#[tokio::test]
async fn arm_with_access_ceremony_inherits_the_choice() {
    use punktfunk_core::quic::{GRANT_ALL, GRANT_GAMEPAD};
    let np = Arc::new(
        crate::native_pairing::NativePairing::load_with(
            Some(std::env::temp_dir().join(format!("pf-mgmt-arm-acc-{}.json", std::process::id()))),
            None,
            false,
        )
        .unwrap(),
    );
    let app = test_app_native(test_state(), np.clone());

    let (s, _) = send(
        &app,
        json_req(
            Method::POST,
            "/api/v1/native/pair/arm",
            serde_json::json!({"grants": GRANT_ALL | (1u32 << 29)}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(!np.status().armed, "a 400 must not arm the window");

    let now = wall_now();
    let (s, b) = send(
        &app,
        json_req(
            Method::POST,
            "/api/v1/native/pair/arm",
            serde_json::json!({"ttl_secs": 60, "grants": GRANT_GAMEPAD, "expires_in_secs": 3600}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(b["armed"], true);
    let carried = np.armed_access().expect("the window carries the choice");
    assert_eq!(carried.grants, GRANT_GAMEPAD);
    let deadline = carried.expires_unix.expect("absolute deadline");
    assert!((now + 3600..=now + 3602).contains(&deadline));

    // Ceremony consumes `armed_access()`; pairing under it inherits the window's choice.
    np.add_with_access("Guest Deck", "dd44", np.armed_access())
        .unwrap();
    assert_eq!(np.effective("dd44", now), Some(GRANT_GAMEPAD));
    let (_, list) = send(&app, get_req("/api/v1/native/clients")).await;
    assert_eq!(list[0]["access_level"], "controller");
    assert_eq!(list[0]["expires_unix"].as_i64().unwrap(), deadline);
}

/// Omit the access fields: store `None`, derive `full` (legacy permanent record).
#[tokio::test]
async fn approve_and_arm_without_access_fields_keep_todays_behavior() {
    let np = Arc::new(
        crate::native_pairing::NativePairing::load_with(
            Some(
                std::env::temp_dir()
                    .join(format!("pf-mgmt-acc-compat-{}.json", std::process::id())),
            ),
            None,
            false,
        )
        .unwrap(),
    );
    let app = test_app_native(test_state(), np.clone());

    let (s, _) = send(
        &app,
        json_req(
            Method::POST,
            "/api/v1/native/pair/arm",
            serde_json::json!({"ttl_secs": 60}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(np.armed_access(), None, "no fields = no choice");

    np.note_pending("Old Laptop", "ee55", Some(LAN_KNOCK), None);
    let (_, pend) = send(&app, get_req("/api/v1/native/pending")).await;
    let id = pend[0]["id"].as_u64().unwrap();
    let (s, b) = send(
        &app,
        json_req(
            Method::POST,
            &format!("/api/v1/native/pending/{id}/approve"),
            serde_json::json!({"name": "Old Laptop"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert!(
        b["grants"].is_null(),
        "no choice = the absent-grants record"
    );
    assert!(b["expires_unix"].is_null());
    assert!(b["granted_unix"].is_null());
    assert_eq!(b["access_level"], "full", "absent grants read as full");
    let stored = &np.list()[0];
    assert_eq!(stored.grants, None);
    assert_eq!(stored.expires_unix, None);
    assert_eq!(stored.granted_unix, None);
}

#[tokio::test]
async fn native_endpoints_report_disabled_without_native_host() {
    let app = test_app(test_state(), None);
    let (s, b) = send(&app, get_req("/api/v1/native/pair")).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(b["enabled"], false);
    let (s, _) = send(
        &app,
        json_req(
            Method::POST,
            "/api/v1/native/pair/arm",
            serde_json::json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
    // Pending list is `[]`, not 503 (same as `/native/clients`).
    let (s, b) = send(&app, get_req("/api/v1/native/pending")).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(b.as_array().unwrap().len(), 0);
    let (s, _) = send(
        &app,
        json_req(
            Method::POST,
            "/api/v1/native/pending/0/approve",
            serde_json::json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
    let (s, _) = send(
        &app,
        json_req(
            Method::POST,
            "/api/v1/native/pending/0/deny",
            serde_json::json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
}
