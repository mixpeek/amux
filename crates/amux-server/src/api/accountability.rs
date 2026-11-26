//! Human-message -> tracked-work accountability (AMUX-2986): the rollup at
//! `GET /api/messages/accountability` and the periodic sweep that nudges a
//! lane holding human messages and no board activity.
//!
//! It reads `cmd_history` and `issues` only. It lived in `api/messages.rs`
//! beside the `_amux_messages` store, which the primitive integrity audit
//! (2026-11-26) removed; this module is what of that file was still in use.

use super::internal;
use super::AppState;
use crate::db::WriteOutcome;
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use chrono::Utc;
use rusqlite::{params, Connection};
use serde::Deserialize;
use serde_json::{json, Value};

/// The rollup keeps its path (`/api/messages/accountability`); nothing else
/// of `/api/messages` remains.
pub fn routes() -> Router<AppState> {
    Router::new().route("/api/messages/accountability", get(accountability))
}

// ---- GET /api/messages/accountability -------------------------------------
//
// "Did every human message become executed work accountable by a worker?"
// (Ethan, 2026-08-12; AMUX-2985/2986/2987). The honest answer needs a JOIN
// nothing exposed: human messages live in `cmd_history` (type='user', empty
// origin — the [HH:MM] dashboard sends; a scheduler carries origin=<title>, an
// agent carries origin=<session>), and the work lives on the board (`issues`).
//
// There is NO per-message->card link today (cmd_history.card_id is NULL for
// 100% of human messages), so this cannot claim "ask X was done by card Y".
// What it CAN do, honestly, is the PROXY: per target worker, count human
// messages in the window against cards that worker CREATED or MOVED in the same
// window, and flag a worker that received messages but produced zero board
// activity. That flag is the self-surfacing signal (ethos rule 4) the drive
// loop can nudge on — the manual cross-reference that produced the first audit
// was exactly the instrument that should not have to be hand-run.

#[derive(Deserialize)]
pub struct AccountabilityParams {
    /// Look-back window in hours (default 24).
    #[serde(default = "default_since_h")]
    pub since_h: u64,
}

fn default_since_h() -> u64 {
    24
}

/// A worker that got human messages in the window but produced no board card.
pub(crate) struct Unaccounted {
    pub worker: String,
    pub human_messages: u64,
    pub latest_snippet: String,
}

/// The full accountability rollup — shared by the HTTP endpoint and the
/// automatic nudge sweep so the two cannot compute "unaccounted" differently
/// (the ethos duplication rule: one definition, two consumers).
pub(crate) struct Rollup {
    pub total_human_messages: u64,
    pub total_linked: u64,
    pub workers: Vec<Value>,
    pub unaccounted: Vec<Unaccounted>,
}

pub(crate) fn compute_rollup(conn: &Connection, since_h: u64) -> rusqlite::Result<Rollup> {
    let now_s = Utc::now().timestamp();
    let cutoff_s = now_s - (since_h as i64) * 3600;
    let cutoff_ms = cutoff_s * 1000; // cmd_history.ts is MILLISECONDS; issues.* are seconds.

    #[derive(Default)]
    struct Row {
        msgs: u64,
        actionable: u64,
        linked: u64,
        latest_ts_ms: i64,
        latest_snippet: String,
        latest_actionable_ts_ms: i64,
        latest_actionable_snippet: String,
        created: u64,
        moved: u64,
    }
    let mut rows: std::collections::BTreeMap<String, Row> = std::collections::BTreeMap::new();

    let mut stmt = conn.prepare(
        "SELECT session, ts, card_id, text \
         FROM cmd_history \
         WHERE type='user' AND origin='' AND ts >= ?1 \
         ORDER BY ts DESC",
    )?;
    let mut q = stmt.query(params![cutoff_ms])?;
    while let Some(r) = q.next()? {
        // session is NOT NULL in cmd_history, but read defensively — a NULL
        // anywhere here 500s the endpoint (issues.session IS nullable).
        let session: String = r.get::<_, Option<String>>(0)?.unwrap_or_default();
        if session.is_empty() {
            continue;
        }
        let ts: i64 = r.get(1)?;
        let card_id: Option<String> = r.get(2)?;
        let message: String = r.get(3)?;
        let snippet: String = message.chars().take(80).collect::<String>().replace(['\n', '\r'], " ");
        let e = rows.entry(session).or_default();
        e.msgs += 1;
        if card_id.as_deref().map(|c| !c.is_empty()).unwrap_or(false) {
            e.linked += 1;
        }
        if ts > e.latest_ts_ms {
            e.latest_ts_ms = ts;
            e.latest_snippet = snippet;
        }
        // A delivered answer-only message or explicit [no-board] control does
        // not owe the worker a task. The board capture path uses this same
        // predicate; the accountability nudge must not contradict it by
        // demanding a new card for a prompt the harness intentionally left
        // cardless.
        if amux_core::board::title_from_prompt(&message).is_some()
            && !amux_core::board::is_informational_query(&message)
            && !amux_core::board::is_conversational_ack(&message)
        {
            e.actionable += 1;
            if ts > e.latest_actionable_ts_ms {
                e.latest_actionable_ts_ms = ts;
                e.latest_actionable_snippet = message.chars().take(80).collect::<String>().replace(['\n', '\r'], " ");
            }
        }
    }

    let mut bstmt = conn.prepare(
        "SELECT session, \
                SUM(CASE WHEN created >= ?1 THEN 1 ELSE 0 END), \
                SUM(CASE WHEN updated >= ?1 THEN 1 ELSE 0 END) \
         FROM issues \
         WHERE COALESCE(deleted,0)=0 AND (created >= ?1 OR updated >= ?1) \
         GROUP BY session",
    )?;
    let mut bq = bstmt.query(params![cutoff_s])?;
    while let Some(r) = bq.next()? {
        // issues.session is nullable — a NULL here is what 500'd the first live
        // call. Skip it: an ownerless card is not a worker's tracked work.
        let Some(session) = r.get::<_, Option<String>>(0)? else {
            continue;
        };
        if let Some(e) = rows.get_mut(&session) {
            e.created = r.get::<_, i64>(1)? as u64;
            e.moved = r.get::<_, i64>(2)? as u64;
        }
    }

    let mut workers = Vec::new();
    let mut unaccounted = Vec::new();
    let mut total_human_messages = 0u64;
    let mut total_linked = 0u64;
    for (session, e) in &rows {
        total_human_messages += e.msgs;
        total_linked += e.linked;
        // A worker with messages but zero board activity has no tracked work to
        // show for the asks. It is a PROXY: a terse "continue"/"do it", or a
        // board-less personal lane, lands here legitimately — a prompt to look,
        // not a verdict of failure.
        let accounted = e.created > 0 || e.moved > 0;
        workers.push(json!({
            "worker": session,
            "human_messages": e.msgs,
            "actionable_messages": e.actionable,
            "messages_linked_to_a_card": e.linked,
            "cards_created_in_window": e.created,
            "cards_moved_in_window": e.moved,
            "latest_message_snippet": e.latest_snippet,
            "verdict": if accounted { "tracking" } else if e.actionable == 0 { "message-only" } else { "no-board-activity" },
        }));
        if !accounted && e.actionable > 0 {
            unaccounted.push(Unaccounted {
                worker: session.clone(),
                human_messages: e.actionable,
                latest_snippet: e.latest_actionable_snippet.clone(),
            });
        }
    }
    workers.sort_by(|a, b| {
        b["human_messages"]
            .as_u64()
            .unwrap_or(0)
            .cmp(&a["human_messages"].as_u64().unwrap_or(0))
    });
    unaccounted.sort_by_key(|b| std::cmp::Reverse(b.human_messages));

    Ok(Rollup {
        total_human_messages,
        total_linked,
        workers,
        unaccounted,
    })
}

pub async fn accountability(
    State(state): State<AppState>,
    Query(p): Query<AccountabilityParams>,
) -> Response {
    let since_h = p.since_h.clamp(1, 24 * 30);
    let store = state.store.clone();
    let joined = tokio::task::spawn_blocking(move || -> anyhow::Result<Value> {
        let conn = store.read()?;
        let r = compute_rollup(&conn, since_h)?;
        let unaccounted: Vec<Value> = r
            .unaccounted
            .iter()
            .map(|u| {
                json!({"worker": u.worker, "human_messages": u.human_messages,
                "latest_message_snippet": u.latest_snippet})
            })
            .collect();
        Ok(json!({
            "since_h": since_h,
            "total_human_messages": r.total_human_messages,
            "total_linked_to_a_card": r.total_linked,
            "linkage_note": "cmd_history.card_id is the ONLY hard link from an ask to its work; \
                             it is ~0 today, so `verdict` is a board-activity PROXY, not proof a \
                             specific ask was executed. The durable fix is stamping card_id when a \
                             worker opens a card from a message (AMUX-2986).",
            "workers": r.workers,
            "unaccounted": unaccounted,
            "unaccounted_count": r.unaccounted.len(),
        }))
    })
    .await;
    match joined {
        Ok(Ok(v)) => Json(v).into_response(),
        Ok(Err(e)) => internal(e),
        Err(e) => internal(e),
    }
}

// ---- automatic accountability nudge (AMUX-2990) ---------------------------
//
// Ethan, 2026-08-12: "the accountability shit needs to be automatic." A server
// background sweep finds unaccounted lanes and STEERS each one directly to open
// a card and pursue it — server-side delivery reaches any lane regardless of
// group, which `amux` (a worker) cannot do. Deduped to at most one nudge per
// lane per cooldown (default 24h, Ethan's chosen cadence) via a persisted
// `accountability_nudged` prefs map, so a standing gap is one nudge/day, not one
// every tick.

const NUDGE_PREFS_KEY: &str = "accountability_nudged";

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// One sweep: nudge every lane that has been unaccounted longer than the
/// cooldown. Public(crate) so a test can drive it against a seeded store.
pub(crate) async fn accountability_tick(state: &AppState) {
    let since_h = env_u64("AMUX_ACCOUNTABILITY_WINDOW_H", 24);
    let cooldown_s = env_u64("AMUX_ACCOUNTABILITY_NUDGE_COOLDOWN_H", 24) as i64 * 3600;
    let now = Utc::now().timestamp();

    let store = state.store.clone();
    let gaps =
        tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<(String, u64, String)>> {
            let conn = store.read()?;
            let r = compute_rollup(&conn, since_h)?;
            Ok(r.unaccounted
                .into_iter()
                .map(|u| (u.worker, u.human_messages, u.latest_snippet))
                .collect())
        })
        .await;
    let gaps = match gaps {
        Ok(Ok(g)) => g,
        Ok(Err(e)) => {
            tracing::warn!(error=%e, "[accountability] sweep query failed");
            return;
        }
        Err(e) => {
            tracing::warn!(error=%e, "[accountability] sweep task panicked");
            return;
        }
    };
    if gaps.is_empty() {
        return;
    }

    // The dedup map: {worker -> last nudge unix-seconds}. A missing worker means
    // never nudged. Read once; write once at the end with the new stamps.
    let mut nudged: std::collections::HashMap<String, i64> = {
        let store = state.store.clone();
        tokio::task::spawn_blocking(move || {
            store
                .read()
                .ok()
                .and_then(|c| {
                    c.query_row(
                        "SELECT value FROM prefs WHERE key=?1",
                        params![NUDGE_PREFS_KEY],
                        |r| r.get::<_, String>(0),
                    )
                    .ok()
                })
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_default()
        })
        .await
        .unwrap_or_default()
    };

    let mut sent = 0usize;
    for (worker, msgs, snippet) in gaps {
        // ISOLATED (AMUX-3232): a raw agent leaves no board trace on purpose and
        // has no session/URL to open a card, so the accountability nudge would be
        // both impossible to satisfy and harness noise inside a lane meant to run
        // untouched. Skip it, reading the same source of truth the spawn path reads.
        if crate::api::session_verbs::session_is_isolated(&worker)
            || crate::api::session_verbs::is_chat_companion(&worker)
        {
            // A Chat-tab companion is the owner's conversation: a nudge typed
            // into it lands as a chat message (AMHC-1 was opened that way).
            continue;
        }
        let last = nudged.get(&worker).copied().unwrap_or(0);
        if now - last < cooldown_s {
            continue; // within cooldown — one nudge/day, not one/tick
        }
        let text = format!(
            "[amux accountability] You have {msgs} message(s) from Ethan in the last {since_h}h with \
             no board card created or moved — the work isn't tracked yet. Please open a board card \
             for the ask (owned by you) and pursue it. Most recent: \"{snippet}\"",
        );
        if let Err(e) =
            crate::api::session_verbs::steer_enqueue(state, &worker, &text, "accountability", "")
                .await
        {
            tracing::warn!(worker=%worker, error=?e, verdict="accountability_nudge_failed",
                "[accountability] nudge could not be queued; the lane retries next sweep");
            continue;
        }
        nudged.insert(worker.clone(), now);
        sent += 1;
        tracing::info!(worker=%worker, human_messages=msgs, "[accountability] nudged unaccounted lane");
    }
    if sent == 0 {
        return;
    }

    // Persist the updated stamps so the next sweep respects the cooldown.
    let body = serde_json::to_string(&nudged).unwrap_or_else(|_| "{}".into());
    if let Err(e) = state
        .store
        .write_async(move |conn| {
            conn.execute(
                "INSERT INTO prefs (key, value) VALUES (?1, ?2) \
                 ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                params![NUDGE_PREFS_KEY, body],
            )?;
            Ok(WriteOutcome {
                applied: true,
                events: vec![],
            })
        })
        .await
    {
        tracing::warn!(error=?e, verdict="accountability_cooldown_write_failed",
            "[accountability] nudge cooldown stamps were not saved; a lane may be nudged again early");
    }
    tracing::info!(
        nudged = sent,
        "[accountability] sweep nudged unaccounted lanes"
    );
}

/// Register the periodic sweep. Interval default 30m; the per-lane cooldown
/// (24h) is what actually bounds how often any one lane hears from it.
pub fn accountability_spawn(state: AppState) -> crate::runtime_jobs::PeriodicTask {
    let secs = env_u64("AMUX_ACCOUNTABILITY_SWEEP_SECS", 1800);
    crate::runtime_jobs::spawn_periodic(
        crate::runtime_jobs::registry::ids::ACCOUNTABILITY_NUDGE,
        secs,
        move || {
            let state = state.clone();
            async move {
                accountability_tick(&state).await;
            }
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::router;
    use crate::db::{SharedStore, Store};
    use axum::body::Body;
    use axum::http::{header, Request, StatusCode};
    use std::sync::Arc;
    use tower::ServiceExt;

    fn app() -> (axum::Router, SharedStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(&dir.path().join("amux-test.db")).unwrap());
        let state = AppState {
            store: store.clone(),
            started: std::time::Instant::now(),
            build_hash: "test".into(),
            auth_token: None,
            reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        };
        (router(state), store, dir)
    }

    async fn send(
        app: &axum::Router,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let b = Request::builder().method(method).uri(path);
        let req = match body {
            Some(v) => b
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(v.to_string()))
                .unwrap(),
            None => b.body(Body::empty()).unwrap(),
        };
        let res = app.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        let v = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes)
                .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()))
        };
        (status, v)
    }

    #[tokio::test]
    async fn accountability_tick_nudges_the_uncooled_lane_and_skips_the_cooled_one() {
        let (_app, store, _dir) = app();
        let state = AppState {
            store: store.clone(),
            started: std::time::Instant::now(),
            build_hash: "test".into(),
            auth_token: None,
            reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        };
        let now_s = Utc::now().timestamp();
        let now_ms = now_s * 1000;
        // AF-188: the enqueue refuses a target with no env file, so both lanes
        // must be REGISTERED for this test to exercise nudging rather than
        // refusal. Before the widening it queued for lanes that never existed,
        // which is precisely the shape being stopped.
        let _home = crate::api::settings::test_env::set_home(_dir.path());
        std::fs::create_dir_all(_dir.path().join("sessions")).unwrap();
        for w in ["w-gap", "w-cooled"] {
            std::fs::write(
                _dir.path().join(format!("sessions/{w}.env")),
                "CC_DIR=/tmp\nCC_AUTO_PICKUP=1\n",
            )
            .unwrap();
        }
        // Both lanes are unaccounted (a human message, no board card). w-cooled
        // was "already nudged just now" via the prefs stamp; w-gap never was.
        store
            .write(move |conn| {
                for w in ["w-gap", "w-cooled"] {
                    conn.execute(
                        "INSERT INTO cmd_history (text,type,session,ts,origin) VALUES ('do the thing','user',?1,?2,'')",
                        params![w, now_ms],
                    )
                    .unwrap();
                }
                conn.execute(
                    "INSERT INTO prefs (key,value) VALUES (?1,?2)",
                    params![NUDGE_PREFS_KEY, format!("{{\"w-cooled\":{now_s}}}")],
                )
                .unwrap();
                Ok(WriteOutcome { applied: true, events: vec![] })
            })
            .unwrap();

        accountability_tick(&state).await;

        let steers = |w: &str| -> i64 {
            store
                .read()
                .unwrap()
                .query_row(
                    "SELECT COUNT(*) FROM steering_queue WHERE session=?1",
                    params![w],
                    |r| r.get(0),
                )
                .unwrap()
        };
        assert_eq!(
            steers("w-gap"),
            1,
            "an un-nudged unaccounted lane must be steered"
        );
        assert_eq!(
            steers("w-cooled"),
            0,
            "a lane nudged within the cooldown must be skipped"
        );
    }

    #[tokio::test]
    async fn accountability_flags_a_worker_with_messages_but_no_board_activity() {
        let (app, store, _dir) = app();
        let now_s = Utc::now().timestamp();
        let now_ms = now_s * 1000;
        // w-gap: got a human message, produced no board card. w-ok: got a human
        // message AND created a card in-window. w-sched: a SCHEDULER message
        // (origin set) that must be excluded from the human count.
        store
            .write(move |conn| {
                let ins_msg = |session: &str, origin: &str, ty: &str, ts: i64| {
                    conn.execute(
                        "INSERT INTO cmd_history (text, type, session, ts, origin) VALUES (?1,?2,?3,?4,?5)",
                        params![format!("ask for {session}"), ty, session, ts, origin],
                    )
                    .unwrap();
                };
                ins_msg("w-gap", "", "user", now_ms - 1000);
                ins_msg("w-ok", "", "user", now_ms - 1000);
                ins_msg("w-sched", "Daily thing", "user", now_ms - 1000); // origin set -> not human
                conn.execute("INSERT INTO cmd_history(text,type,session,ts,origin,delivery,delivered_at) VALUES('[11:17 AM] [no-board] Reply with ACK only','user','w-message-only',?1,'','direct',?1)",params![now_ms-1000]).unwrap();
                // A board card w-ok created just now; w-gap has none.
                conn.execute(
                    "INSERT INTO issues (id,title,status,session,created,updated) VALUES ('I-OK','t','todo','w-ok',?1,?1)",
                    params![now_s],
                )
                .unwrap();
                // An OWNERLESS card (session NULL) in-window — this 500'd the
                // first live call (issues.session is nullable). The endpoint must
                // skip it, not choke on the NULL.
                conn.execute(
                    "INSERT INTO issues (id,title,status,session,created,updated) VALUES ('I-NULL','t','todo',NULL,?1,?1)",
                    params![now_s],
                )
                .unwrap();
                Ok(WriteOutcome { applied: true, events: vec![] })
            })
            .unwrap();

        let (st, v) = send(&app, "GET", "/api/messages/accountability?since_h=24", None).await;
        assert_eq!(st, StatusCode::OK, "{v}");
        // The no-board prompt remains a human message, but cannot trigger a
        // work-card nudge; the scheduler is excluded from human messages.
        assert_eq!(v["total_human_messages"], json!(3), "{v}");
        let unacc: Vec<&str> = v["unaccounted"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|u| u["worker"].as_str())
            .collect();
        assert_eq!(
            unacc,
            vec!["w-gap"],
            "only the worker with no board activity is flagged: {v}"
        );
        assert_eq!(v["workers"].as_array().unwrap().iter().find(|w|w["worker"]=="w-message-only").unwrap()["verdict"],json!("message-only"));
        // And w-ok reads as tracking, not flagged.
        let ok_row = v["workers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|w| w["worker"] == "w-ok")
            .unwrap();
        assert_eq!(ok_row["verdict"], json!("tracking"));
        assert_eq!(ok_row["cards_created_in_window"], json!(1));
    }
}
