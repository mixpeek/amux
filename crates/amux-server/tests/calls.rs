//! Outbound calls end to end against a fake call bridge (api/calls.rs,
//! docs/design/phone-calls.md).
//!
//! The real router serves on a local port; a tiny axum server plays the bridge:
//! it takes `POST /calls`, answers with a provider sid, then calls amux back on
//! the `events_url` and `result_url` it was given, with the `result_token` it was
//! given, replaying a scripted conversation. Everything amux owes after that is
//! asserted: the status walk, the token check, the owner-only approval, the
//! transcript file, the CRM interaction and the `call.completed` event.
//!
//! One test function on purpose: AMUX_HOME is process-wide, and this binary
//! holds nothing else that reads it.

use amux_server::api::{router, AppState};
use amux_server::db::Store;
use axum::{extract::State, routing::post, Json, Router};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Clone, Default)]
struct Bridge {
    received: Arc<Mutex<Vec<Value>>>,
    callbacks: Arc<Mutex<Vec<(String, u16)>>>,
}

async fn fake_bridge_calls(State(b): State<Bridge>, Json(body): Json<Value>) -> Json<Value> {
    b.received.lock().unwrap().push(body.clone());
    let b2 = b.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        let http = reqwest::Client::new();
        let token = body["result_token"].as_str().unwrap().to_string();
        let events = body["events_url"].as_str().unwrap().to_string();
        let result = body["result_url"].as_str().unwrap().to_string();
        for ev in ["ringing", "answered", "ended"] {
            let r = http.post(&events).bearer_auth(&token).json(&json!({"event": ev})).send().await.unwrap();
            b2.callbacks.lock().unwrap().push((format!("event:{ev}"), r.status().as_u16()));
        }
        let r = http
            .post(&result)
            .bearer_auth(&token)
            .json(&json!({
                "status": "completed",
                "outcome": "booked: table for two at 7pm Friday",
                "summary": "The host confirmed a table for two on Friday at 7pm under Steininger.",
                "duration_s": 74.4,
                "transcript": [
                    {"who": "agent", "text": "Hi, I'm an AI assistant calling on behalf of Ethan Steininger.", "t": 0.5},
                    {"who": "callee", "text": "Sure, how can I help?", "t": 4.0},
                    {"who": "agent", "text": "I'd like a table for two this Friday at 7pm.", "t": 6.2},
                    {"who": "callee", "text": "Done, see you Friday.", "t": 12.9}
                ]
            }))
            .send()
            .await
            .unwrap();
        b2.callbacks.lock().unwrap().push(("result".into(), r.status().as_u16()));
    });
    Json(json!({"provider_sid": "CA-fake-0001"}))
}

async fn serve(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app.into_make_service_with_connect_info::<std::net::SocketAddr>()).await.unwrap();
    });
    format!("http://{addr}")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_held_call_is_approved_dialed_and_reported_back() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::env::set_var("AMUX_HOME", &home);

    let bridge = Bridge::default();
    let bridge_url = serve(Router::new().route("/calls", post(fake_bridge_calls)).with_state(bridge.clone())).await;

    let store = Arc::new(Store::open(&dir.path().join("calls.db")).unwrap());
    let state = AppState {
        store: store.clone(),
        started: std::time::Instant::now(),
        build_hash: "test".into(),
        auth_token: None,
        reconciled: Arc::new(std::sync::atomic::AtomicBool::new(true)),
    };
    let amux = serve(router(state)).await;
    // server.env wins over the process env (settings::effective_env), so the
    // test is not steered by whatever this shell exports.
    std::fs::write(
        home.join("server.env"),
        format!("AMUX_CALL_BRIDGE_URL={bridge_url}\nAMUX_CALL_RESULT_BASE={amux}\nAMUX_CALL_ALLOWLIST=+15550000001\nTWILIO_FROM=+15550009999\n"),
    )
    .unwrap();

    let http = reqwest::Client::new();
    let get = |path: String| {
        let http = http.clone();
        let amux = amux.clone();
        async move { http.get(format!("{amux}{path}")).send().await.unwrap().json::<Value>().await.unwrap() }
    };

    // A contact owns the number, so the call is logged against them.
    let contact: Value = http
        .post(format!("{amux}/api/crm/contacts"))
        .json(&json!({"name": "Luigi's Trattoria", "phone": "(555) 010-2030"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let contact_id = contact["id"].as_str().unwrap().to_string();

    // A worker asks; the number is not on the allowlist, so it is held.
    let r = http
        .post(format!("{amux}/api/calls"))
        .header("X-Amux-Session", "test-caller")
        .json(&json!({"to": "555-010-2030", "goal": "Book a table for two this Friday at 7pm", "context": "Name: Steininger", "max_minutes": 5}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 202, "a call outside the allowlist is held");
    let held: Value = r.json().await.unwrap();
    let id = held["id"].as_str().unwrap().to_string();
    assert_eq!(held["status"], "held");
    assert_eq!(held["call"]["to_number"], "+15550102030", "the number is stored in E.164");
    assert_eq!(held["call"]["contact"], contact_id.as_str(), "the CRM contact is matched by number");
    assert!(held["call"].get("result_token_hash").is_none(), "the token hash never leaves the server");
    println!("PASS held: {id} to +15550102030 waits for the owner");

    // Nothing dials while it is held.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(bridge.received.lock().unwrap().is_empty(), "a held call never reaches the bridge");

    // A worker can never approve, its own call or another's.
    for who in ["test-caller", "some-other-worker"] {
        let r = http.post(format!("{amux}/api/calls/{id}/approve")).header("X-Amux-Session", who).send().await.unwrap();
        assert_eq!(r.status().as_u16(), 403, "{who} cannot approve");
    }
    assert_eq!(get(format!("/api/calls/{id}")).await["status"], "held", "a refused approval changes nothing");
    println!("PASS a worker cannot approve");

    // The owner approves; amux dials the bridge with the contract's fields.
    let r = http.post(format!("{amux}/api/calls/{id}/approve")).send().await.unwrap();
    assert_eq!(r.status().as_u16(), 200);
    let approved: Value = r.json().await.unwrap();
    assert_eq!(approved["call"]["provider_sid"], "CA-fake-0001", "the bridge's sid is stored: {approved}");
    let sent = bridge.received.lock().unwrap().first().cloned().expect("the bridge got the call");
    assert_eq!(sent["call_id"], id.as_str());
    assert_eq!(sent["to"], "+15550102030");
    assert_eq!(sent["from"], "+15550009999");
    assert_eq!(sent["max_minutes"], 5);
    let instructions = sent["instructions"].as_str().unwrap();
    assert!(
        instructions.starts_with("You are an AI assistant calling on behalf of "),
        "the disclosure is the first sentence: {instructions}"
    );
    assert!(instructions.contains("Book a table for two"));
    assert!(instructions.contains("Name: Steininger"));
    assert_eq!(sent["result_url"], format!("{amux}/api/calls/{id}/result").as_str());
    let token = sent["result_token"].as_str().unwrap().to_string();
    assert_eq!(token.len(), 64);
    println!("PASS dialed: the bridge got the disclosure-first instructions and a per-call token");

    // A wrong token, or none, is refused.
    let r = http
        .post(format!("{amux}/api/calls/{id}/result"))
        .bearer_auth("0".repeat(64))
        .json(&json!({"status": "completed", "outcome": "forged"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 401, "a wrong token is refused");
    let r = http.post(format!("{amux}/api/calls/{id}/events")).json(&json!({"event": "answered"})).send().await.unwrap();
    assert_eq!(r.status().as_u16(), 401, "no token is refused");
    println!("PASS a report without this call's token is refused");

    // Wait for the bridge's scripted result.
    let mut done = Value::Null;
    for _ in 0..100 {
        done = get(format!("/api/calls/{id}")).await;
        if done["status"] == "completed" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(done["status"], "completed", "the call completed: {done}; bridge saw {:?}", bridge.callbacks.lock().unwrap());
    assert_ne!(done["outcome"], "forged", "the forged result never landed");
    assert_eq!(done["outcome"], "booked: table for two at 7pm Friday");
    assert_eq!(done["duration_s"], 74);
    assert_eq!(done["transcript"].as_array().unwrap().len(), 4);
    assert_eq!(done["contact_name"], "Luigi's Trattoria");
    assert!(done["started_at"].as_f64().is_some(), "the answered event set started_at");
    let cbs = bridge.callbacks.lock().unwrap().clone();
    assert!(cbs.iter().all(|(_, s)| *s == 200), "every bridge callback was accepted: {cbs:?}");
    println!("PASS completed: outcome, summary, duration and 4 transcript turns stored");

    // The transcript file.
    let path = home.join("calls").join(format!("{id}.md"));
    let md = std::fs::read_to_string(&path).expect("transcript file written");
    assert!(md.contains("Done, see you Friday."), "{md}");
    assert!(md.contains("booked: table for two at 7pm Friday"));
    assert_eq!(done["transcript_path"], path.to_string_lossy().as_ref());
    println!("PASS transcript written to {}", path.display());

    // The CRM interaction.
    let c = get(format!("/api/crm/contacts/{contact_id}")).await;
    let ix = c["interactions"].as_array().unwrap();
    assert!(
        ix.iter().any(|i| i["type"] == "call" && i["notes"].as_str().unwrap_or("").contains(&id)),
        "a CRM interaction of type call names the call: {ix:?}"
    );
    println!("PASS CRM interaction logged on {contact_id}");

    // The events, in order, on the requesting worker.
    let events: Vec<(String, String, Value)> = store
        .read_async(move |conn| {
            let mut st = conn.prepare("SELECT session, type, data FROM session_events WHERE source = 'calls' ORDER BY id")?;
            let rows = st
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows.into_iter().map(|(s, t, d)| (s, t, serde_json::from_str(&d).unwrap_or(Value::Null))).collect())
        })
        .await
        .unwrap();
    let types: Vec<&str> = events.iter().map(|(_, t, _)| t.as_str()).collect();
    assert_eq!(types, ["call.requested", "call.held", "call.started", "call.completed"], "{events:?}");
    let (session, _, data) = events.last().unwrap();
    assert_eq!(session, "test-caller");
    assert_eq!(data["status"], "completed");
    assert_eq!(data["contact"], contact_id.as_str());
    assert_eq!(data["to"], "+15550102030");
    println!("PASS call.completed emitted for test-caller");

    // A result is taken once.
    let r = http
        .post(format!("{amux}/api/calls/{id}/result"))
        .bearer_auth(&token)
        .json(&json!({"status": "failed"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 409, "a second result for an ended call is refused");

    // The list carries it, filtered by worker and status.
    let list = get("/api/calls?worker=test-caller&status=completed".into()).await;
    assert_eq!(list["count"], 1);
    assert_eq!(list["measured"], true);
    assert_eq!(list["bridge_configured"], true);
    assert!(list["calls"][0].get("transcript").is_none(), "the list stays light");
    assert_eq!(get("/api/calls?status=held".into()).await["count"], 0);

    // An allowlisted number skips the hold and dials at once.
    let r = http
        .post(format!("{amux}/api/calls"))
        .header("X-Amux-Session", "test-caller")
        .json(&json!({"to": "+1 555 000 0001", "goal": "Leave a reminder about the dentist"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 201);
    let quick: Value = r.json().await.unwrap();
    assert_eq!(quick["call"]["approved_by"], "allowlist");
    assert_ne!(quick["status"], "held");

    // A declined call never dials and still tells the worker it ended.
    let r: Value = http
        .post(format!("{amux}/api/calls"))
        .header("X-Amux-Session", "test-caller")
        .json(&json!({"to": "+44 20 7946 0958", "goal": "Ask about opening hours"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let declined = r["id"].as_str().unwrap().to_string();
    let n_before = bridge.received.lock().unwrap().len();
    let r = http.post(format!("{amux}/api/calls/{declined}/decline")).header("X-Amux-Session", "test-caller").send().await.unwrap();
    assert_eq!(r.status().as_u16(), 403, "a worker cannot decline either");
    let r = http.post(format!("{amux}/api/calls/{declined}/decline")).send().await.unwrap();
    assert_eq!(r.status().as_u16(), 200);
    assert_eq!(get(format!("/api/calls/{declined}")).await["status"], "declined");
    assert_eq!(bridge.received.lock().unwrap().len(), n_before, "a declined call never reaches the bridge");

    // Bad input is refused with a reason.
    let r = http.post(format!("{amux}/api/calls")).json(&json!({"to": "call me", "goal": "x"})).send().await.unwrap();
    assert_eq!(r.status().as_u16(), 400);
    let r = http.post(format!("{amux}/api/calls")).json(&json!({"to": "+15550102030", "goal": " "})).send().await.unwrap();
    assert_eq!(r.status().as_u16(), 400);
    println!("PASS allowlist skips the hold, decline is owner-only, bad input is refused");

    // No bridge configured: the call fails with the reason, and says so.
    std::fs::write(home.join("server.env"), "AMUX_CALL_BRIDGE_URL=\nAMUX_CALL_ALLOWLIST=+15550000001\n").unwrap();
    let r = http
        .post(format!("{amux}/api/calls"))
        .header("X-Amux-Session", "test-caller")
        .json(&json!({"to": "+15550000001", "goal": "Say hello"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 502);
    let failed: Value = r.json().await.unwrap();
    assert_eq!(failed["status"], "failed");
    assert_eq!(failed["error"], "no call bridge configured (AMUX_CALL_BRIDGE_URL)");
    let fid = failed["id"].as_str().unwrap();
    assert!(home.join("calls").join(format!("{fid}.md")).exists(), "a failed call still leaves its record");
    assert_eq!(get("/api/calls".into()).await["bridge_configured"], false);
    println!("PASS no bridge: the call fails with the reason");
}
