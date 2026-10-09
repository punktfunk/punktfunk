//! The log ring, the SSE event stream and hooks.

use super::*;

#[tokio::test]
async fn logs_endpoint_pages_by_cursor() {
    let app = test_app(test_state(), None);

    // Process-wide ring; other tests log into it. Assert on our markers inside the page,
    // never that the page is exactly ours.
    let (s, json) = send(&app, get_req("/api/v1/logs")).await;
    assert_eq!(s, StatusCode::OK);
    let start = json["next"].as_u64().unwrap();

    let ring = crate::log_capture::ring();
    ring.push(&tracing::Level::WARN, "mgmt::tests", "first".into());
    ring.push(&tracing::Level::INFO, "mgmt::tests", "second".into());

    let (s, json) = send(&app, get_req(&format!("/api/v1/logs?after={start}"))).await;
    assert_eq!(s, StatusCode::OK);
    let entries = json["entries"].as_array().unwrap();
    let ours: Vec<_> = entries
        .iter()
        .filter(|e| e["target"] == "mgmt::tests")
        .collect();
    assert_eq!(ours.len(), 2, "both markers on the page, in order");
    assert_eq!(ours[0]["msg"], "first");
    assert_eq!(ours[0]["level"], "WARN");
    assert_eq!(ours[1]["msg"], "second");
    let next = json["next"].as_u64().unwrap();
    assert_eq!(
        next,
        start + entries.len() as u64,
        "the cursor advances by exactly the entries served"
    );
    assert_eq!(json["dropped"], false);

    // Concurrent tests may add entries; our already-served markers must not appear again.
    let (s, json) = send(&app, get_req(&format!("/api/v1/logs?after={next}"))).await;
    assert_eq!(s, StatusCode::OK);
    assert!(json["entries"]
        .as_array()
        .unwrap()
        .iter()
        .all(|e| e["target"] != "mgmt::tests"));
    assert!(json["next"].as_u64().unwrap() >= next);
}

async fn next_sse_chunk(body: &mut Body) -> Option<String> {
    match tokio::time::timeout(std::time::Duration::from_secs(5), body.frame()).await {
        Ok(Some(Ok(frame))) => frame
            .into_data()
            .ok()
            .map(|b| String::from_utf8_lossy(&b).into_owned()),
        _ => None,
    }
}

fn sse_data_events(text: &str) -> Vec<serde_json::Value> {
    text.lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter_map(|d| serde_json::from_str(d).ok())
        .collect()
}

#[tokio::test]
async fn events_stream_requires_bearer() {
    let app = test_app(test_state(), None);
    let mut req = get_req("/api/v1/events");
    req.headers_mut().insert(
        axum::http::header::AUTHORIZATION,
        axum::http::HeaderValue::from_static("Bearer wrong"),
    );
    let resp = app.clone().oneshot(req).await.expect("infallible");
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

/// Ring catch-up, kind filter, live tail, `?since=` / `Last-Event-ID` resume, and `dropped`
/// when the cursor fell off the ring.
#[tokio::test]
async fn events_stream_catch_up_filter_resume_tail_and_dropped() {
    use crate::events::EventKind;
    let _l = EVENTS_TEST_LOCK.lock().await;
    let app = test_app(test_state(), None);
    let uniq = format!("evt-{}", std::process::id());
    let m1 = format!("{uniq}-one");

    crate::events::emit(EventKind::DisplayReleased { count: 424_242 });
    crate::events::emit(EventKind::LibraryChanged { source: m1.clone() });

    let resp = app
        .clone()
        .oneshot(with_bearer(
            get_req("/api/v1/events?kinds=library.changed"),
            "test-secret",
        ))
        .await
        .expect("infallible");
    assert_eq!(resp.status(), StatusCode::OK);
    let ctype = resp
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(
        ctype.starts_with("text/event-stream"),
        "content-type: {ctype}"
    );

    // Catch-up must deliver m1; other tests' library.changed events may interleave — scan.
    let mut body = resp.into_body();
    let mut seen = String::new();
    while !seen.contains(&m1) {
        let chunk = next_sse_chunk(&mut body)
            .await
            .expect("catch-up delivers the marker event");
        seen.push_str(&chunk);
    }
    assert!(
        !seen.contains("event: display.released"),
        "kind filter must drop other kinds: {seen}"
    );
    assert!(
        seen.contains("event: library.changed"),
        "frame kind: {seen}"
    );
    let m1_seq = sse_data_events(&seen)
        .iter()
        .find(|e| e["source"] == m1.as_str())
        .and_then(|e| e["seq"].as_u64())
        .expect("marker frame carries the full event JSON with its seq");

    // Live tail on the same connection. If a concurrent flood cuts the slow consumer, reconnect
    // with the last seen id (the documented client move) instead of flaking.
    let m2 = format!("{uniq}-two");
    crate::events::emit(EventKind::LibraryChanged { source: m2.clone() });
    let mut tail = String::new();
    loop {
        match next_sse_chunk(&mut body).await {
            Some(chunk) => {
                tail.push_str(&chunk);
                if tail.contains(&m2) {
                    break;
                }
            }
            None => {
                let resp = app
                    .clone()
                    .oneshot(with_bearer(
                        get_req(&format!(
                            "/api/v1/events?since={m1_seq}&kinds=library.changed"
                        )),
                        "test-secret",
                    ))
                    .await
                    .expect("infallible");
                body = resp.into_body();
            }
        }
    }
    drop(body);
    // The `live` marker closes the catch-up: it must come after m1, never before it.
    let all = format!("{seen}{tail}");
    let live_at = all
        .find("event: live")
        .unwrap_or_else(|| panic!("no live marker: {all}"));
    assert!(
        live_at > all.find(&m1).unwrap(),
        "live before catch-up: {all}"
    );

    let resp = app
        .clone()
        .oneshot(with_bearer(
            get_req(&format!(
                "/api/v1/events?since={m1_seq}&kinds=library.changed"
            )),
            "test-secret",
        ))
        .await
        .expect("infallible");
    let mut body = resp.into_body();
    let mut resumed = String::new();
    while !resumed.contains(&m2) {
        let chunk = next_sse_chunk(&mut body)
            .await
            .expect("resume catch-up delivers m2");
        resumed.push_str(&chunk);
    }
    assert!(!resumed.contains(&m1), "since-cursor must exclude m1");
    drop(body);

    // Last-Event-ID beats `?since` (newer cursor on SSE auto-reconnect).
    let mut req = with_bearer(
        get_req("/api/v1/events?since=0&kinds=library.changed"),
        "test-secret",
    );
    req.headers_mut().insert(
        "last-event-id",
        axum::http::HeaderValue::from_str(&m1_seq.to_string()).unwrap(),
    );
    let resp = app.clone().oneshot(req).await.expect("infallible");
    let mut body = resp.into_body();
    let mut resumed = String::new();
    while !resumed.contains(&m2) {
        let chunk = next_sse_chunk(&mut body)
            .await
            .expect("header-resume catch-up delivers m2");
        resumed.push_str(&chunk);
    }
    assert!(!resumed.contains(&m1), "Last-Event-ID must exclude m1");
    drop(body);

    // 1100 > ring capacity; resume from seq 1 must get the dropped marker first.
    for _ in 0..1100 {
        crate::events::emit(EventKind::DisplayReleased { count: 1 });
    }
    let resp = app
        .clone()
        .oneshot(with_bearer(
            get_req("/api/v1/events?since=1"),
            "test-secret",
        ))
        .await
        .expect("infallible");
    let mut body = resp.into_body();
    let first = next_sse_chunk(&mut body).await.expect("dropped marker");
    assert!(first.contains("event: dropped"), "first frame: {first}");
    assert!(
        first.contains(r#"{"dropped":true}"#),
        "marker data: {first}"
    );
}

#[tokio::test]
async fn events_stream_connection_cap() {
    let _l = EVENTS_TEST_LOCK.lock().await;
    let app = test_app(test_state(), None);

    let slots = crate::mgmt::events::test_support::saturate_slots();
    let resp = app
        .clone()
        .oneshot(with_bearer(get_req("/api/v1/events"), "test-secret"))
        .await
        .expect("infallible");
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    drop(slots);

    let resp = app
        .clone()
        .oneshot(with_bearer(get_req("/api/v1/events"), "test-secret"))
        .await
        .expect("infallible");
    assert_eq!(resp.status(), StatusCode::OK, "cap frees with the slots");
}

// ------------------------------------------------------------------ hooks

/// GET shape + PUT validation. A successful PUT would write the real config dir;
/// persistence is unit-tested in `crate::hooks` against a temp path.
#[tokio::test]
async fn hooks_get_shape_and_put_validation() {
    let app = test_app(test_state(), None);

    let (s, json) = send(&app, get_req("/api/v1/hooks")).await;
    assert_eq!(s, StatusCode::OK);
    assert!(json["hooks"].is_array());

    let put = |body: serde_json::Value| {
        axum::http::Request::put("/api/v1/hooks")
            .header(axum::http::header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    };

    let (s, json) = send(
        &app,
        put(serde_json::json!({"hooks": [{"on": "stream.started"}]})),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(
        json["error"].as_str().unwrap().contains("run"),
        "error names the problem: {json}"
    );

    let (s, _) = send(
        &app,
        put(serde_json::json!({"hooks": [{"on": "pairing.*", "webhook": "ftp://x"}]})),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    let mut req = get_req("/api/v1/hooks");
    req.headers_mut().insert(
        axum::http::header::AUTHORIZATION,
        axum::http::HeaderValue::from_static("Bearer wrong"),
    );
    let resp = app.clone().oneshot(req).await.expect("infallible");
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}
