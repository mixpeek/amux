//! Brex tokenized-card HTTP surface. DISABLED by default (see
//! integrations::brex). Three routes: a status read that never leaks the token,
//! a gated card-create, and the transaction webhook that runs the four-window
//! budget guard and freezes the card the moment a window would be crossed.

use super::AppState;
use crate::db::WriteOutcome;
use crate::integrations::brex::{BrexClient, BrexConfig, Tallies, Verdict};
use axum::{extract::State, http::StatusCode, response::IntoResponse, routing::{get, post}, Json, Router};
use serde_json::{json, Value};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/status", get(status))
        .route("/card", post(create_card))
        .route("/webhook", post(webhook))
}

/// Calendar-period cutoffs (unix ms) in LOCAL time: start of today, start of
/// the ISO week (Monday), start of the month. Brex budgets are calendar
/// periods, so the guard's windows match what the human set on the card.
fn window_cutoffs(now: chrono::DateTime<chrono::Local>) -> (i64, i64, i64) {
    use chrono::{Datelike, TimeZone};
    let ms = |d: chrono::DateTime<chrono::Local>| d.timestamp_millis();
    let midnight = chrono::Local
        .with_ymd_and_hms(now.year(), now.month(), now.day(), 0, 0, 0)
        .single()
        .unwrap_or(now);
    let week_start = midnight - chrono::Duration::days(now.weekday().num_days_from_monday() as i64);
    let month_start = chrono::Local
        .with_ymd_and_hms(now.year(), now.month(), 1, 0, 0, 0)
        .single()
        .unwrap_or(midnight);
    (ms(midnight), ms(week_start), ms(month_start))
}

fn tallies_for(conn: &rusqlite::Connection, card_id: &str, now: chrono::DateTime<chrono::Local>) -> rusqlite::Result<Tallies> {
    let (day, week, month) = window_cutoffs(now);
    let sum = |cutoff: i64| -> rusqlite::Result<i64> {
        // The table is created lazily by the webhook; before the first charge it
        // may not exist, which is zero spend, not an error.
        match conn.query_row(
            "SELECT COALESCE(SUM(amount_cents),0) FROM _amux_brex_spend WHERE card_id=?1 AND posted_at>=?2",
            rusqlite::params![card_id, cutoff],
            |r| r.get(0),
        ) {
            Ok(v) => Ok(v),
            Err(rusqlite::Error::SqliteFailure(_, Some(ref m))) if m.contains("no such table") => Ok(0),
            Err(e) => Err(e),
        }
    };
    Ok(Tallies { today: sum(day)?, week: sum(week)?, month: sum(month)? })
}

async fn status(State(state): State<AppState>) -> Response {
    let cfg = BrexConfig::from_env();
    let card = cfg.card_id.clone();
    let spend = match (&card, tokio::task::spawn_blocking({
        let store = state.store.clone();
        let card = card.clone();
        move || -> anyhow::Result<Option<Tallies>> {
            let Some(card) = card else { return Ok(None) };
            let conn = store.read()?;
            Ok(Some(tallies_for(&conn, &card, chrono::Local::now())?))
        }
    }).await) {
        (_, Ok(Ok(Some(t)))) => json!({"today_cents": t.today, "week_cents": t.week, "month_cents": t.month}),
        _ => json!(null),
    };
    Json(json!({
        "measured": true, "n_considered": 1,
        "enabled": cfg.enabled, "sandbox": cfg.sandbox, "live": cfg.live(),
        "has_token": cfg.token.is_some(), "card_id": cfg.card_id,
        "limits_cents": {"per_txn": cfg.limits.per_txn, "daily": cfg.limits.daily, "weekly": cfg.limits.weekly, "monthly": cfg.limits.monthly},
        "spend": spend,
        "note": "Scaffolding: no card is issued and no money moves until AMUX_BREX_ENABLED=1 and AMUX_BREX_TOKEN are set. Verify request bodies against Brex's live API first.",
    })).into_response()
}

type Response = axum::response::Response;

async fn create_card(State(_state): State<AppState>, body: Option<Json<Value>>) -> Response {
    let cfg = BrexConfig::from_env();
    if !cfg.live() {
        return (StatusCode::CONFLICT, Json(json!({"ok": false, "error": "brex disabled; set AMUX_BREX_ENABLED=1 and AMUX_BREX_TOKEN", "measured": true, "n_considered": 1}))).into_response();
    }
    let holder = body.as_ref().and_then(|b| b.0.get("holder_name")).and_then(Value::as_str).unwrap_or("worker").to_string();
    match BrexClient::new(cfg.clone()).create_virtual_card(&holder, cfg.limits.monthly).await {
        Ok(v) => Json(json!({"ok": true, "card": v})).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, Json(json!({"ok": false, "error": e.to_string()}))).into_response(),
    }
}

/// Brex transaction webhook. Records the charge idempotently (UNIQUE txn_id),
/// then runs the budget guard on the post-charge window totals; a freeze verdict
/// locks the card via the API and is logged with its computed reason.
async fn webhook(State(state): State<AppState>, Json(body): Json<Value>) -> Response {
    let cfg = BrexConfig::from_env();
    // Accept both a bare transaction and Brex's {data:{...}} envelope.
    let tx = body.get("data").unwrap_or(&body);
    let txn_id = tx.get("id").and_then(Value::as_str).unwrap_or("").to_string();
    let card_id = tx.get("card_id").and_then(Value::as_str).or(cfg.card_id.as_deref()).unwrap_or("").to_string();
    // Brex amounts are in the account currency's minor units; accept a couple of shapes.
    let amount_cents = tx.get("amount").and_then(|a| a.get("amount")).and_then(Value::as_i64)
        .or_else(|| tx.get("amount_cents").and_then(Value::as_i64))
        .unwrap_or(0);
    let merchant = tx.get("merchant").and_then(|m| m.get("raw_descriptor").or_else(|| m.get("name"))).and_then(Value::as_str).unwrap_or("").to_string();
    let posted_at = tx.get("posted_at_date").and_then(Value::as_str).and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok()).map(|d| d.timestamp_millis())
        .unwrap_or_else(|| chrono::Utc::now().timestamp_millis());
    if txn_id.is_empty() || card_id.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({"ok": false, "error": "webhook missing transaction id or card id", "measured": true, "n_considered": 1}))).into_response();
    }

    let now = chrono::Utc::now().timestamp_millis();
    let (rec_card, rec_txn) = (card_id.clone(), txn_id.clone());
    let inserted = state.store.write_async(move |conn| {
        // Lazy schema: this scaffolding owns no migration (the migration array
        // is a hot, order-sensitive shared file and a numbering collision there
        // is worse than a CREATE IF NOT EXISTS here). When the feature graduates
        // from disabled scaffolding to real, give it a proper numbered migration.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS _amux_brex_spend (
                id INTEGER PRIMARY KEY, txn_id TEXT NOT NULL UNIQUE, card_id TEXT NOT NULL,
                amount_cents INTEGER NOT NULL, currency TEXT NOT NULL DEFAULT 'USD',
                merchant TEXT NOT NULL DEFAULT '', posted_at INTEGER NOT NULL, created_at INTEGER NOT NULL);
             CREATE INDEX IF NOT EXISTS idx_brex_spend_card_time ON _amux_brex_spend(card_id, posted_at);")?;
        let n = conn.execute(
            "INSERT OR IGNORE INTO _amux_brex_spend (txn_id,card_id,amount_cents,currency,merchant,posted_at,created_at)
             VALUES (?1,?2,?3,'USD',?4,?5,?6)",
            rusqlite::params![rec_txn, rec_card, amount_cents, merchant, posted_at, now],
        )?;
        Ok(WriteOutcome { applied: n > 0, events: vec![] })
    }).await;
    if let Err(e) = inserted {
        tracing::warn!(target: "amux::brex", %e, "brex webhook: could not record transaction");
        return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"ok": false, "error": "could not record transaction"}))).into_response();
    }

    // Evaluate the four-window guard on the totals INCLUDING this charge.
    let eval_card = card_id.clone();
    let verdict = tokio::task::spawn_blocking({
        let store = state.store.clone();
        move || -> anyhow::Result<Verdict> {
            let conn = store.read()?;
            let tallies = tallies_for(&conn, &eval_card, chrono::Local::now())?;
            // tallies already include this charge (it was inserted above), so the
            // window check compares pre-charge totals against the limit; subtract
            // this charge back out to get the "before" figure the guard expects.
            let before = Tallies {
                today: tallies.today - amount_cents.max(0),
                week: tallies.week - amount_cents.max(0),
                month: tallies.month - amount_cents.max(0),
            };
            Ok(BrexConfig::from_env().guard().evaluate(before, amount_cents))
        }
    }).await.map(|r| r.unwrap_or(Verdict::Allow)).unwrap_or(Verdict::Allow);

    if let Verdict::Freeze { dimension, limit_cents, would_be_cents } = verdict {
        tracing::warn!(target: "amux::brex", card_id=%card_id, dimension, limit_cents, would_be_cents, txn_id=%txn_id,
            measured=true, n_considered=1, "brex budget guard: freezing card: window would be crossed");
        if cfg.live() {
            let reason = format!("amux budget guard: {dimension} cap {limit_cents}c would become {would_be_cents}c");
            if let Err(e) = BrexClient::new(cfg.clone()).freeze_card(&card_id, &reason).await {
                tracing::warn!(target: "amux::brex", %e, card_id=%card_id, "brex freeze call failed after guard verdict");
            }
        }
    }

    Json(json!({"ok": true, "recorded": true, "verdict": verdict, "measured": true, "n_considered": 1})).into_response()
}
