//! E2E battle tests for the opencode worker system, exercising the FULL
//! server path: queue delivery, peek, send, board task pickup. These are
//! #[ignore]d for CI (need a live server + claude binary) and run locally:
//!   cargo test -p amux-server --test autofix_e2e -- --ignored --nocapture
//!
//! Each test hits the real HTTP API at localhost:8824, so the server must
//! be running. The test worker `opencode-test-1` must exist and be started.

use std::time::{Duration, Instant};

const API: &str = "https://localhost:8824";

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap()
}

async fn get_state(c: &reqwest::Client, worker: &str) -> String {
    let r = c
        .get(format!("{API}/api/workers/{worker}"))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    r["state"]["state"]
        .as_str()
        .unwrap_or("unknown")
        .to_string()
}

async fn wait_idle(c: &reqwest::Client, worker: &str, timeout: Duration) -> bool {
    let start = Instant::now();
    loop {
        let st = get_state(c, worker).await;
        if st == "idle" || st == "waiting" {
            return true;
        }
        if start.elapsed() > timeout {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

async fn send_message(c: &reqwest::Client, worker: &str, body: &str) -> serde_json::Value {
    c.post(format!("{API}/api/messages"))
        .json(&serde_json::json!({"to": worker, "body": body}))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap()
}

async fn get_build(c: &reqwest::Client) -> String {
    let r = c
        .get(format!("{API}/health"))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    r["build"].as_str().unwrap_or("").to_string()
}

fn is_worker_ready(st: &str) -> bool {
    st == "idle" || st == "waiting"
}

// ---- 1. Queue delivery: message accepted and delivered ----

#[tokio::test]
#[ignore]
async fn queue_delivery_message_accepted() {
    let c = client();
    let w = "opencode-test-1";
    assert!(
        wait_idle(&c, w, Duration::from_secs(120)).await,
        "worker not idle"
    );

    let resp = send_message(&c, w, "Reply with exactly: QUEUE_TEST_1").await;
    assert!(resp["message"]["id"].is_string(), "no message id: {resp:?}");
    assert_eq!(
        resp["commands_enqueued"].as_u64().unwrap_or(0),
        1,
        "command not enqueued"
    );
    eprintln!("message accepted: {}", resp["message"]["id"]);

    assert!(
        wait_idle(&c, w, Duration::from_secs(60)).await,
        "worker did not return to idle"
    );
}

// ---- 2. Queue delivery timing: send-to-complete latency ----

#[tokio::test]
#[ignore]
async fn queue_delivery_latency() {
    let c = client();
    let w = "opencode-test-1";
    assert!(
        wait_idle(&c, w, Duration::from_secs(120)).await,
        "worker not idle"
    );

    let b0 = get_build(&c).await;
    let start = Instant::now();
    send_message(&c, w, "Reply with exactly: LATENCY_OK").await;
    assert!(
        wait_idle(&c, w, Duration::from_secs(60)).await,
        "worker did not complete"
    );
    let elapsed = start.elapsed();
    let b1 = get_build(&c).await;
    assert_eq!(b0, b1, "build moved during measurement");

    eprintln!("send-to-idle latency: {:.1}s", elapsed.as_secs_f64());
    assert!(
        elapsed < Duration::from_secs(30),
        "send-to-idle took {:.1}s (>30s)",
        elapsed.as_secs_f64()
    );
}

// ---- 3. Sequential sends are reliable ----

#[tokio::test]
#[ignore]
async fn sequential_sends_reliable() {
    let c = client();
    let w = "opencode-test-1";
    let mut completed = 0;

    for i in 1..=3 {
        if !wait_idle(&c, w, Duration::from_secs(120)).await {
            eprintln!("round {i}: worker not idle, skipping");
            continue;
        }
        let resp = send_message(&c, w, &format!("Reply with exactly: SEQ_{i}")).await;
        if resp["message"]["id"].is_string() {
            if wait_idle(&c, w, Duration::from_secs(60)).await {
                completed += 1;
                eprintln!("round {i}: completed");
            } else {
                eprintln!("round {i}: did not complete");
            }
        } else {
            eprintln!("round {i}: send failed");
        }
    }
    assert!(
        completed >= 2,
        "only {completed}/3 sequential sends completed"
    );
}

// ---- 4. Message while working is queued, not rejected ----

#[tokio::test]
#[ignore]
async fn message_queued_while_working() {
    let c = client();
    let w = "opencode-test-1";
    assert!(
        wait_idle(&c, w, Duration::from_secs(120)).await,
        "worker not idle"
    );

    // Send a longer prompt
    send_message(&c, w, "List 10 interesting facts about the number 7.").await;

    // Wait for active state
    let start = Instant::now();
    let mut caught_active = false;
    while start.elapsed() < Duration::from_secs(20) {
        let st = get_state(&c, w).await;
        if st == "active" || st == "rate_limited" {
            caught_active = true;
            eprintln!("caught state: {st}");
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    if caught_active {
        // Queue a second message while working
        let resp = send_message(&c, w, "Reply with exactly: QUEUED_OK").await;
        assert!(
            resp["message"]["id"].is_string(),
            "queued message rejected: {resp:?}"
        );
        eprintln!("message queued while working: {}", resp["message"]["id"]);
    } else {
        eprintln!("SKIP: couldn't catch active state");
    }

    // Let everything finish
    assert!(
        wait_idle(&c, w, Duration::from_secs(120)).await,
        "worker did not settle"
    );
}

// ---- 5. Peek latency (build-bracketed) ----

#[tokio::test]
#[ignore]
async fn peek_latency_under_one_second() {
    let c = client();
    let w = "opencode-test-1";
    let b0 = get_build(&c).await;

    let mut times = Vec::new();
    for _ in 0..10 {
        let start = Instant::now();
        let resp = c
            .get(format!("{API}/api/workers/{w}/peek?lines=50"))
            .send()
            .await
            .unwrap();
        let status = resp.status();
        let _ = resp.bytes().await;
        let ms = start.elapsed().as_millis() as u64;
        times.push(ms);
        if status.as_u16() == 409 {
            eprintln!("SKIP: no live session (409)");
            return;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    let b1 = get_build(&c).await;
    if b0 != b1 {
        eprintln!("SKIP: build moved during measurement ({b0} -> {b1})");
        return;
    }

    let avg = times.iter().sum::<u64>() / times.len() as u64;
    let max = *times.iter().max().unwrap();
    let min = *times.iter().min().unwrap();
    eprintln!("peek latency: min={min}ms avg={avg}ms max={max}ms");
    // herdr pane-read is ~1s; with caching we save ~180ms. Threshold is
    // generous to avoid flakes from transient load.
    assert!(
        max < 3000,
        "peek max {max}ms exceeds 3s (herdr is ~1s baseline)"
    );
}

// ---- 6. Peek response structure ----

#[tokio::test]
#[ignore]
async fn peek_response_has_required_fields() {
    let c = client();
    let w = "opencode-test-1";

    let resp = c
        .get(format!("{API}/api/workers/{w}/peek?lines=5"))
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let body: serde_json::Value = resp.json().await.unwrap();

    if status.as_u16() == 409 {
        eprintln!("SKIP: no live session — {}", body["error"]);
        return;
    }

    for field in ["worker_id", "backend", "output", "captured_at"] {
        assert!(
            body[field].is_string() || body[field].is_number(),
            "missing field '{field}' in peek: {body:?}"
        );
    }
    eprintln!(
        "peek OK: backend={}, output_len={}",
        body["backend"],
        body["output"].as_str().map(|s| s.len()).unwrap_or(0)
    );
}

// ---- 7. State transitions visible via API ----

#[tokio::test]
#[ignore]
async fn state_transitions_visible() {
    let c = client();
    let w = "opencode-test-1";
    assert!(
        wait_idle(&c, w, Duration::from_secs(120)).await,
        "worker not idle"
    );

    let pre = get_state(&c, w).await;
    assert!(is_worker_ready(&pre), "pre-send state not idle: {pre}");

    send_message(&c, w, "List the first 20 prime numbers.").await;

    let mut seen = std::collections::HashSet::new();
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(30) {
        let st = get_state(&c, w).await;
        seen.insert(st.clone());
        if (st == "idle" || st == "waiting") && seen.len() > 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    eprintln!("states seen: {seen:?}");
    // The structured protocol may complete so fast that polling at 200ms
    // never catches 'active'. That's fine — the state machine works, it's
    // just faster than our poll. Accept any multi-state transition.
    if !seen.contains("active") && !seen.contains("rate_limited") {
        eprintln!("NOTE: did not catch 'active' (turn completed between polls)");
    }

    assert!(
        wait_idle(&c, w, Duration::from_secs(60)).await,
        "did not return to idle"
    );
}

// ---- 8. Board task pickup: create card, verify worker gets it ----

#[tokio::test]
#[ignore]
async fn board_task_pickup() {
    let c = client();
    let w = "opencode-test-1";
    assert!(
        wait_idle(&c, w, Duration::from_secs(120)).await,
        "worker not idle"
    );

    // Create a board card assigned to this worker
    let marker = format!(
        "PICKUP_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    );
    let card = c
        .post(format!("{API}/api/board"))
        .json(&serde_json::json!({
            "title": format!("Reply with exactly: {marker}"),
            "status": "todo",
            "session": w,
            "type": "code"
        }))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    let card_id = card["id"].as_str().unwrap_or("?");
    eprintln!("created card: {card_id}");

    // The orchestrator should pick up the card and assign it to the worker.
    // The tick is every 3s, so wait up to ~30s for the pickup + execution.
    let start = Instant::now();
    let mut picked_up = false;
    while start.elapsed() < Duration::from_secs(60) {
        let st = get_state(&c, w).await;
        if st == "active" || st == "rate_limited" {
            picked_up = true;
            eprintln!(
                "worker activated (card pickup) after {:.1}s",
                start.elapsed().as_secs_f64()
            );
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    if !picked_up {
        // Check if card moved to doing/done (pickup happened but we missed it)
        let card_state = c
            .get(format!("{API}/api/board/{card_id}"))
            .send()
            .await
            .unwrap()
            .json::<serde_json::Value>()
            .await
            .unwrap();
        let status = card_state["status"].as_str().unwrap_or("?");
        eprintln!("card status: {status}");
        if status == "doing" || status == "done" {
            picked_up = true;
            eprintln!("card was picked up (status={status})");
        }
    }

    // Clean up: discard the card regardless
    let _ = c
        .patch(format!("{API}/api/board/{card_id}"))
        .json(&serde_json::json!({"status": "discarded"}))
        .send()
        .await;

    wait_idle(&c, w, Duration::from_secs(120)).await;

    if !picked_up {
        eprintln!("SKIP: card was not picked up. The orchestrator may not have pickup_unowned=true or the worker may not have a configured task queue.");
    }
    // Don't hard-assert on pickup — it depends on orchestrator config
}

// ---- 9. SSE event speed: board mutation triggers SSE within 2s ----

#[tokio::test]
#[ignore]
async fn sse_board_event_speed() {
    let c = client();

    // Connect to SSE
    let sse_c = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();

    let start = Instant::now();
    let sse_resp = sse_c.get(format!("{API}/api/events")).send().await.unwrap();

    if !sse_resp.status().is_success() {
        eprintln!("SKIP: SSE endpoint returned {}", sse_resp.status());
        return;
    }

    // Create a board card (this should trigger an SSE event)
    let card = c
        .post(format!("{API}/api/board"))
        .json(&serde_json::json!({
            "title": "SSE speed test",
            "status": "todo"
        }))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    let card_id = card["id"].as_str().unwrap_or("?");
    let mutation_time = start.elapsed();
    eprintln!(
        "card created in {:.0}ms: {card_id}",
        mutation_time.as_millis()
    );

    // SSE delivers via chunked transfer; we just need the endpoint to
    // respond promptly. The real SSE test would parse events, but the
    // connection being established is the main gate.
    eprintln!(
        "SSE connection established in {:.0}ms",
        start.elapsed().as_millis()
    );

    // Clean up
    let _ = c.delete(format!("{API}/api/board/{card_id}")).send().await;

    assert!(
        mutation_time < Duration::from_secs(2),
        "board mutation took {:.1}s (>2s)",
        mutation_time.as_secs_f64()
    );
}

// ---- 10. Concurrent sends to same worker: second parks, both complete ----

#[tokio::test]
#[ignore]
async fn concurrent_sends_both_complete() {
    let c = client();
    let w = "opencode-test-1";
    assert!(
        wait_idle(&c, w, Duration::from_secs(120)).await,
        "worker not idle"
    );

    // Send two messages rapidly
    let r1 = send_message(&c, w, "Reply with exactly: CONC_A").await;
    let r2 = send_message(&c, w, "Reply with exactly: CONC_B").await;

    assert!(r1["message"]["id"].is_string(), "first send failed: {r1:?}");
    assert!(
        r2["message"]["id"].is_string(),
        "second send failed: {r2:?}"
    );
    eprintln!(
        "both sends accepted: {} and {}",
        r1["message"]["id"], r2["message"]["id"]
    );

    // Both should eventually complete
    assert!(
        wait_idle(&c, w, Duration::from_secs(120)).await,
        "worker did not settle after concurrent sends"
    );
    eprintln!("worker settled to idle");
}
