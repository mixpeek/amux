//! GET /api/observability — the Cost tab's rollup over `token_ledger`
//! (AMUX-2893). Python contract: `observability_rollup` py:18237.
//!
//! Response keys are exactly what `_costRender` reads: `total_cost`,
//! `total_tokens`, `total_turns`, `cache_hit_pct`, `by_task[{title,cost,
//! tokens,turns}]`, `by_session`, `by_model`, `by_day[{day,cost,tokens}]`.
//!
//! TWO DEPARTURES from Python, both because of what porting this uncovered:
//!
//! 1. It does NOT index on request. Python called `_index_token_ledger()` at
//!    the top (py:18245), which made the READER the writer's trigger — the
//!    ledger only advanced while someone had this tab open. That is how it went
//!    36 hours stale with `/api/stats/daily` serving a confident zero
//!    (AMUX-2892). The periodic job owns indexing now.
//!
//! 2. Because of (1), the response carries `ledger_fresh_through` and
//!    `ledger_stale_s`. A rollup over a dead ledger renders perfectly and shows
//!    small numbers, and small is indistinguishable from quiet — so the one
//!    thing a caller cannot work out for itself is how old the underlying data
//!    is. Now it can, and the client says so.
//!
//! `?group=` resolves members through `history::group_members`, the SAME
//! predicate `/api/board?group=` and `/api/history?group=` use. Not re-derived:
//! a cost figure that disagrees with the board about who is in a group is the
//! kind of wrong number nobody re-checks before believing it.

use super::AppState;
use crate::api::fs::{parse_qs, qs_get};
use crate::api::history::group_members;
use crate::api::settings::amux_home;
use axum::extract::{RawQuery, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{json, Value};

pub fn routes() -> Router<AppState> {
    Router::new().route("/api/observability", get(rollup))
}

/// The token columns that make up a turn's total, spelled once. Python repeats
/// `input+cache_read+cache_write+output` in four queries; one drifting copy is
/// a silently wrong total.
const TOKENS: &str = "input+cache_read+cache_write+output";

async fn rollup(State(state): State<AppState>, RawQuery(q): RawQuery) -> Response {
    let p = parse_qs(q.as_deref().unwrap_or(""));
    let days: i64 = qs_get(&p, "days")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(7)
        .clamp(1, 365);
    let session = qs_get(&p, "session").unwrap_or_default().trim().to_string();
    let group = qs_get(&p, "group").unwrap_or_default().trim().to_string();

    // `?session=` wins over `?group=` (Python: group applies only without a
    // session), and an unknown group must total ZERO rather than falling
    // through to the fleet — a wrong dollar figure under a group name is one
    // nobody re-derives before believing it (py:18260).
    let members: Vec<String> = if session.is_empty() && !group.is_empty() {
        group_members(&amux_home(), &group)
    } else {
        vec![]
    };
    let scope_is_empty_group = session.is_empty() && !group.is_empty() && members.is_empty();

    let since = chrono::Utc::now().timestamp() - days * 86400;

    let conn = match state.store.read() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": e.to_string()})),
            )
                .into_response()
        }
    };

    let mut where_sql = String::from("ts >= ?1");
    let mut args: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(since)];
    if !session.is_empty() {
        where_sql.push_str(" AND session = ?2");
        args.push(Box::new(session.clone()));
    } else if scope_is_empty_group {
        where_sql.push_str(" AND 1=0");
    } else if !members.is_empty() {
        let holes: Vec<String> = (0..members.len()).map(|i| format!("?{}", i + 2)).collect();
        where_sql.push_str(&format!(" AND session IN ({})", holes.join(",")));
        for m in &members {
            args.push(Box::new(m.clone()));
        }
    }
    let params: Vec<&dyn rusqlite::ToSql> = args.iter().map(|b| b.as_ref()).collect();

    let one = |sql: String| -> Option<Vec<f64>> {
        conn.query_row(&sql, params.as_slice(), |r| {
            let mut v = Vec::new();
            let mut i = 0;
            while let Ok(x) = r.get::<_, f64>(i) {
                v.push(x);
                i += 1;
            }
            Ok(v)
        })
        .ok()
    };

    let tot = one(format!(
        "SELECT COALESCE(SUM(cost_usd),0), COALESCE(SUM({TOKENS}),0), \
         COALESCE(SUM(cache_read),0), COALESCE(SUM(input+cache_read+cache_write),0), \
         COUNT(*) FROM token_ledger WHERE {where_sql}"
    ))
    .unwrap_or_else(|| vec![0.0; 5]);
    let (total_cost, total_tokens, cache_read, total_in_all, total_turns) =
        (tot[0], tot[1] as i64, tot[2], tot[3], tot[4] as i64);

    // Delegated (subagent) spend, kept SEPARATE rather than silently folded in.
    // Subagent turns only started being counted on 2026-08-11 (AMUX-2894), so a
    // lane's cost visibly jumps; a caller that cannot see which part is
    // delegated has no way to tell that jump from a real spending change.
    let delegated = one(format!(
        "SELECT COALESCE(SUM(cost_usd),0), COUNT(*) FROM token_ledger \
         WHERE {where_sql} AND conversation LIKE 'agent-%'"
    ))
    .unwrap_or_else(|| vec![0.0; 2]);

    let group_rows = |select: &str, group_by: &str, order: &str| -> Vec<Vec<Value>> {
        let sql = format!(
            "SELECT {select} FROM token_ledger WHERE {where_sql} GROUP BY {group_by} ORDER BY {order}"
        );
        // SILENCE-VS-ZERO (AMUX-2974): a prepare/query FAILURE renders as an
        // empty breakdown — indistinguishable from "no spend" — on the Cost
        // view. We keep the empty-Vec so the UI degrades gracefully (the totals
        // computed elsewhere still show), but the swallow is now AUDIBLE: a
        // WARN naming the group makes a broken breakdown visible in
        // /api/logs/analyze and a log sweep, instead of a Cost view that lies
        // quiet (ethos rule 4 — the instrument must be able to express the
        // failure).
        let mut stmt = match conn.prepare(&sql) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(target: "amux::observability", %group_by, error = %e,
                    "cost breakdown query failed to PREPARE — the Cost view will show an empty \
                     '{group_by}' breakdown that looks like no spend");
                return vec![];
            }
        };
        let rows = stmt.query_map(params.as_slice(), |r| {
            let mut out = Vec::new();
            let mut i = 0;
            while let Ok(v) = r.get_ref(i) {
                out.push(match v {
                    rusqlite::types::ValueRef::Text(t) => {
                        json!(String::from_utf8_lossy(t).to_string())
                    }
                    rusqlite::types::ValueRef::Integer(n) => json!(n),
                    rusqlite::types::ValueRef::Real(f) => json!(f),
                    _ => Value::Null,
                });
                i += 1;
            }
            Ok(out)
        });
        match rows {
            Ok(it) => it.flatten().collect(),
            Err(e) => {
                tracing::warn!(target: "amux::observability", %group_by, error = %e,
                    "cost breakdown query failed to RUN — empty '{group_by}' breakdown shown as \
                     if there were no spend");
                vec![]
            }
        }
    };

    let by_session: Vec<Value> = group_rows(
        &format!("session, COALESCE(SUM(cost_usd),0) cost, SUM({TOKENS}) tokens, COUNT(*) turns"),
        "session",
        "cost DESC",
    )
    .into_iter()
    .map(|r| json!({"session": r[0], "cost": r[1], "tokens": r[2], "turns": r[3]}))
    .collect();

    // Task titles come from task_windows (py:18284). A task id with no window
    // row falls back to the id itself rather than rendering blank.
    let titles: std::collections::HashMap<String, String> = conn
        .prepare("SELECT DISTINCT task, title FROM task_windows")
        .and_then(|mut s| {
            let rows = s.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
            Ok(rows.flatten().collect())
        })
        .unwrap_or_default();

    let by_task: Vec<Value> = group_rows(
        &format!("task, COALESCE(SUM(cost_usd),0) cost, SUM({TOKENS}) tokens, COUNT(*) turns"),
        "task",
        "cost DESC",
    )
    .into_iter()
    .map(|r| {
        let task = r[0].as_str().unwrap_or("").to_string();
        let title = if task.is_empty() {
            "Ambient (untasked)".to_string()
        } else {
            titles.get(&task).cloned().unwrap_or_else(|| task.clone())
        };
        json!({"task": task, "title": title, "cost": r[1], "tokens": r[2], "turns": r[3]})
    })
    .collect();

    let by_model_rows = group_rows(
        "model, COALESCE(SUM(cost_usd),0) cost, COUNT(*) turns",
        "model",
        "cost DESC",
    );

    // METERED VS UNMETERED (AMUX-4806). `total_cost` above sums `cost_usd`
    // over every row, and until this landed an unknown model was priced at
    // Anthropic Sonnet's rate by `PRICE_DEFAULT`. 116,966 rows of gpt-*,
    // gemini-* and a local qwen carried $6,942.14 that nobody spent, in the
    // same figure as measured Claude spend, and nothing here could tell them
    // apart.
    //
    // Classified in RUST against the live price table rather than with a
    // status list in SQL, for two reasons: the table is the authority and a
    // second copy in a WHERE clause is the drift this repo keeps paying for,
    // and it follows ~/.amux/prices.json for free — the moment the owner adds
    // a rate, that family moves from unmetered to metered with no redeploy and
    // no backfill.
    //
    // `by_model` is GROUP BY with no LIMIT, so this sum is the whole
    // population, not a head.
    let price_table = crate::runtime_jobs::token_ledger::prices(&crate::config::amux_home());
    let mut unmetered_turns = 0i64;
    let mut unmetered_cost_in_ledger = 0.0f64;
    let mut unmetered_models: Vec<Value> = Vec::new();
    let mut free_turns = 0i64;
    let mut free_cost_in_ledger = 0.0f64;
    for r in &by_model_rows {
        let m = r[0].as_str().unwrap_or_default();
        if m.is_empty() || m == "<synthetic>" {
            continue;
        }
        let cost = r[1].as_f64().unwrap_or(0.0);
        let turns = r[2].as_i64().unwrap_or(0);
        match crate::runtime_jobs::token_ledger::model_rate(&price_table, m) {
            // FREE BY RATE, not by absence. A local model's rate is a real
            // zero, so ANY cost stored against it is a leftover from when
            // every unknown model took Sonnet's price. Counting it as spend
            // would assert $223.32 of bill for 551 turns that ran on this Mac
            // and cost nothing -- the same defect, surviving in a bucket that
            // now calls itself "priced". Cost the row by what the TABLE says.
            Some(rate) if rate.iter().all(|r| *r == 0.0) => {
                free_turns += turns;
                free_cost_in_ledger += cost;
            }
            Some(_) => {}
            None => {
                unmetered_turns += turns;
                unmetered_cost_in_ledger += cost;
                unmetered_models.push(json!({"model": m, "turns": turns, "cost_in_ledger": cost}));
            }
        }
    }
    // The headline is METERED spend. A row whose rate nobody knows contributes
    // its tokens and no dollars; the legacy figure it still carries in the
    // table is reported beside it rather than folded in or deleted.
    let metered_cost = total_cost - unmetered_cost_in_ledger - free_cost_in_ledger;

    let by_model: Vec<Value> = by_model_rows
        .into_iter()
        .map(|r| {
            let priced = r[0]
                .as_str()
                .map(|m| crate::runtime_jobs::token_ledger::model_is_priced(&price_table, m))
                .unwrap_or(false);
            json!({"model": r[0], "cost": r[1], "turns": r[2], "priced": priced})
        })
        .collect();

    let by_day: Vec<Value> = group_rows(
        &format!(
            "strftime('%Y-%m-%d', ts, 'unixepoch', 'localtime') day, \
             COALESCE(SUM(cost_usd),0) cost, SUM({TOKENS}) tokens"
        ),
        "day",
        "day",
    )
    .into_iter()
    .map(|r| json!({"day": r[0], "cost": r[1], "tokens": r[2]}))
    .collect();

    // Ledger freshness is UNSCOPED on purpose: it describes the indexer, not
    // this query's slice. Scoping it would make a quiet worker look like a
    // broken pipeline.
    let fresh_through: Option<i64> = conn
        .query_row("SELECT MAX(ts) FROM token_ledger", [], |r| r.get(0))
        .ok()
        .flatten();

    Json(json!({
        "days": days,
        "session": if session.is_empty() { Value::Null } else { json!(session) },
        "group": if group.is_empty() { Value::Null } else { json!(group) },
        // METERED ONLY (AMUX-4806). A caller reading this cannot silently
        // include a row priced at a rate nobody supplied.
        "total_cost": (metered_cost * 10_000.0).round() / 10_000.0,
        "unmetered_turns": unmetered_turns,
        // Legacy cost still stored on unmetered rows, from when every unknown
        // model took Sonnet's rate. Disclosed, not deleted: it is history, and
        // overwriting a money column is not reversible.
        "unmetered_cost_in_ledger": (unmetered_cost_in_ledger * 10_000.0).round() / 10_000.0,
        "unmetered_models": unmetered_models,
        // Local inference: a known rate of zero. Reported so "$0 because we
        // measured it" stays distinguishable from "no number to give".
        "free_turns": free_turns,
        "free_cost_in_ledger": (free_cost_in_ledger * 10_000.0).round() / 10_000.0,
        "total_tokens": total_tokens,
        "total_turns": total_turns,
        // py:18293 divides by `or 1` — a zero denominator would make this NaN,
        // and NaN serializes to null, which renders as a blank card rather than
        // an honest 0%.
        "cache_hit_pct": ((100.0 * cache_read / total_in_all.max(1.0)) * 10.0).round() / 10.0,
        "delegated_cost": (delegated[0] * 10_000.0).round() / 10_000.0,
        "delegated_turns": delegated[1] as i64,
        "by_session": by_session,
        "by_task": by_task,
        "by_model": by_model,
        "by_day": by_day,
        "ledger_fresh_through": fresh_through,
        "ledger_stale_s": fresh_through.map(|t| (chrono::Utc::now().timestamp() - t).max(0)),
    }))
    .into_response()
}
