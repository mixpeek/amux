//! RR-0110 — universal search (FTS5), Invariant 32.
//!
//! `GET /api/search?q=&types=&limit=&offset=` returns ranked, typed hits
//! across board cards (title, desc AND the card log), messages, memories,
//! worker metadata, journal entries and schedules. Net-new: neither the
//! Python server nor the SPA ever had a /api/search.
//!
//! The index itself and the triggers that keep it current live in migration
//! `0013_search.sql` — read its header for why the maintenance is SQL triggers
//! rather than a Rust write hook. This module is the query side plus the two
//! instruments that make a wrong answer visible:
//!
//! - `GET /api/search/status` — per-type index counts vs LIVE source-table
//!   counts, and a `consistent` verdict. An index that quietly stopped being
//!   maintained looks exactly like a corpus with nothing in it, so the only
//!   way "search found nothing" is trustworthy is if drift can be seen
//!   (ethos rule 4). This is also the backfill's count report: the migration
//!   cannot print, this endpoint can.
//! - `POST /api/search/reindex` — rebuild from the source tables, returning
//!   before/after counts per type. The repair, walkable from the sanctioned
//!   tooling (`amux-rs search --reindex`), not a hand-rolled sqlite3 session.
//!
//! Two deliberate design points:
//!
//! **The user's query is never handed to FTS5 raw.** `q` is tokenized here and
//! re-emitted as quoted FTS5 terms. Raw pass-through makes half of ordinary
//! typing (`foo:`, `a-b`, an unbalanced `"`) a 500 from the query parser, and
//! the other half — `NOT`, `*`, column filters — silently means something the
//! typist did not intend. Explicit `"phrases in quotes"` survive; everything
//! else is an AND of terms with a prefix match on the last one.
//!
//! **Highlighting cannot inject markup.** FTS5's `snippet()` is asked to wrap
//! matches in two control characters that cannot occur in indexed text; the
//! whole snippet is then HTML-escaped and ONLY those sentinels become
//! `<mark>`/`</mark>`. A snippet containing `<script>` from a card body comes
//! back escaped, with its match still highlighted.

use super::AppState;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use rusqlite::Connection;
use serde::Deserialize;
use serde_json::{json, Value};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/", get(search))
        .route("/status", get(status))
        .route("/reindex", post(reindex))
}

/// Default and maximum page size. The cap exists so one query cannot serialize
/// the whole corpus; `limit_capped` in the response says when it bit.
const DEFAULT_LIMIT: usize = 20;
const MAX_LIMIT: usize = 200;

/// How far the total-matches count is allowed to scan before it gives up and
/// says so. Counting every match of a common term on a 600k-row DB is a full
/// index walk for a number nobody reads past "lots".
const COUNT_CAP: usize = 1000;

/// Sentinels for snippet highlighting: two ASCII control characters. They
/// cannot appear in indexed text, which is what makes the escape-then-replace
/// safe (a body containing the literal string "<mark>" stays escaped).
const HL_OPEN: char = '\u{1}';
const HL_CLOSE: char = '\u{2}';

/// The indexed families, paired with the live table + predicate the index is
/// derived from. `status` compares these two counts; `reindex` rebuilds from
/// the same SELECTs the migration's backfill uses.
///
/// This table is the single place the two mechanisms agree — a status view
/// that re-derived its own predicate would drift from the index it describes,
/// which is the exact failure ethos rule 1 names.
const FAMILIES: &[(&str, &str, &str)] = &[
    ("task", "issues", "deleted IS NULL"),
    ("schedule", "schedules", "deleted IS NULL"),
    ("journal", "journal_entries", "deleted IS NULL"),
    ("memory", "_amux_memories", "deleted_at IS NULL"),
    ("message", "_amux_messages", "1=1"),
    ("worker", "_amux_workers", "1=1"),
    // AF-499. Only type='user': the rest of cmd_history is machine traffic
    // (auto-pickup dispatches, peer relays, scheduler commands) and indexing it
    // would turn the corpus into a log. The predicate is repeated in
    // 0056_search_prompts.sql's trigger and backfill; this table is what makes
    // the status view describe the SAME population the index holds.
    ("prompt", "cmd_history", "type = 'user'"),
];

#[derive(Deserialize, Default)]
pub struct SearchParams {
    #[serde(default)]
    q: String,
    /// Comma-separated entity types; empty = all.
    #[serde(default)]
    types: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    offset: Option<usize>,
}

use super::internal;

// ---------------------------------------------------------------------------
// Query construction
// ---------------------------------------------------------------------------

/// Turn free text into an FTS5 MATCH expression that cannot be a syntax error
/// and cannot mean something the typist did not write.
///
/// - `"quoted phrases"` are preserved as phrases.
/// - every other run of non-space characters becomes a quoted term (so `foo:`,
///   `a-b`, `C++` are literals, not operators);
/// - the LAST bare term gets a `*` so typing is incremental;
/// - terms are ANDed, which is what a person means by typing two words.
///
/// Returns `None` when nothing indexable is left — the caller answers with an
/// empty result and says `q` was empty, rather than running `MATCH ''`
/// (an error) or `MATCH '*'` (everything).
pub fn build_match(q: &str) -> Option<String> {
    let mut terms: Vec<String> = Vec::new();
    let mut chars = q.chars().peekable();
    let mut cur = String::new();
    let mut last_was_phrase = false;
    while let Some(c) = chars.next() {
        if c == '"' {
            if !cur.trim().is_empty() {
                terms.push(quote_term(cur.trim()));
                last_was_phrase = false;
            }
            cur.clear();
            let mut phrase = String::new();
            for c2 in chars.by_ref() {
                if c2 == '"' {
                    break;
                }
                phrase.push(c2);
            }
            // An unterminated quote is treated as "phrase runs to end of
            // input" — the same thing a search box does, and never an error.
            if !phrase.trim().is_empty() {
                terms.push(quote_term(phrase.trim()));
                last_was_phrase = true;
            }
        } else if c.is_whitespace() {
            if !cur.trim().is_empty() {
                terms.push(quote_term(cur.trim()));
                last_was_phrase = false;
            }
            cur.clear();
        } else {
            cur.push(c);
        }
    }
    if !cur.trim().is_empty() {
        terms.push(quote_term(cur.trim()));
        last_was_phrase = false;
    }
    if terms.is_empty() {
        return None;
    }
    // Incremental typing: the trailing bare term is a prefix. A quoted phrase
    // is taken as finished, so it is not turned into a prefix match.
    if !last_was_phrase {
        if let Some(last) = terms.last_mut() {
            last.push('*');
        }
    }
    Some(terms.join(" AND "))
}

/// FTS5 string literal: wrap in double quotes, doubling any inner quote.
/// Characters FTS5 treats as separators are harmless inside a quoted string.
fn quote_term(t: &str) -> String {
    format!("\"{}\"", t.replace('"', "\"\""))
}

/// HTML-escape the snippet, then turn the two sentinels into `<mark>` tags.
/// Order matters: escaping first is what makes the markup non-injectable.
fn render_snippet(raw: &str) -> String {
    let escaped = raw
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;");
    escaped
        .replace(HL_OPEN, "<mark>")
        .replace(HL_CLOSE, "</mark>")
}

fn parse_types(raw: &Option<String>) -> Vec<String> {
    raw.as_deref()
        .map(|s| {
            s.split(',')
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// GET /api/search
// ---------------------------------------------------------------------------

async fn search(State(st): State<AppState>, Query(p): Query<SearchParams>) -> Response {
    let started = std::time::Instant::now();
    let limit_raw = p.limit.unwrap_or(DEFAULT_LIMIT);
    let limit = limit_raw.clamp(1, MAX_LIMIT);
    let offset = p.offset.unwrap_or(0);
    let types = parse_types(&p.types);

    let Some(match_expr) = build_match(&p.q) else {
        return Json(json!({
            "q": p.q,
            "types": types,
            "hits": [],
            "total": 0,
            "total_capped": false,
            "limit": limit,
            "offset": offset,
            "took_ms": 0,
            // Not an error: an empty query has an empty answer, and saying so
            // is different from saying "no matches".
            "note": "empty query: no searchable terms",
        }))
        .into_response();
    };

    let conn = match st.store.read() {
        Ok(c) => c,
        Err(e) => return internal(e),
    };
    match run_search(&conn, &match_expr, &types, limit, offset) {
        Ok((hits, total, total_capped)) => {
            // A ZERO MUST SAY WHETHER THE MEASUREMENT RAN (TG-3303, ethos rule 4).
            //
            // ts-gke searched for a card they had created two hours earlier,
            // got `{"hits":[]}` with HTTP 200, and were one step from re-filing
            // a peer's work as untracked. Only a known-positive control told
            // them the instrument was dead rather than the data absent — and
            // `/api/board` REFUSES `q=` with a 400 that redirects here, so the
            // documented way to find a card by content was the one that could
            // silently answer "nothing".
            //
            // A silently-empty search does not fail, it AGREES WITH YOU. Every
            // "no prior art exists, I will build it" decision made through it
            // is then unfalsifiable from the outside.
            //
            // So the zero path, and only the zero path, pays one COUNT: an
            // index with no documents cannot answer any query, and returning
            // 200 with an empty list is the lie. That case is now a 503.
            let indexed = if hits.is_empty() { index_doc_count(&conn) } else { None };
            if hits.is_empty() && indexed == Some(0) {
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(json!({
                        "error": "search index is empty — this is a broken index, not a query with no matches",
                        "q": p.q,
                        "index_docs": 0,
                        "hits": [],
                        "total": 0,
                        "fix": "POST /api/search/reindex rebuilds it; GET /api/search/status reports \
                                per-family index vs live counts",
                        "card": "TG-3303",
                    })),
                )
                    .into_response();
            }
            Json(json!({
                "q": p.q,
                "match": match_expr,
                "types": types,
                "hits": hits,
                "total": total,
                "total_capped": total_capped,
                "limit": limit,
                "limit_capped": limit_raw > MAX_LIMIT,
                "offset": offset,
                "took_ms": started.elapsed().as_millis() as u64,
                // Present ONLY on a zero-hit answer, which is the only time a
                // caller needs it: it is the difference between "I looked
                // through 10,423 documents and none matched" and "I have
                // nothing to look through". Null means the count itself could
                // not be taken, which is a third answer and not a zero.
                "index_docs": indexed,
            }))
            .into_response()
        }
        Err(e) => internal(e),
    }
}

/// How many documents the index holds, or `None` if the count could not be
/// taken. `None` is deliberately not `0`: "I could not measure" and "there is
/// nothing there" send a reader to different places.
fn index_doc_count(conn: &Connection) -> Option<i64> {
    conn.query_row("SELECT COUNT(*) FROM search_docs", [], |r| r.get::<_, i64>(0)).ok()
}

/// The ranked query. bm25 weights `title` 10x `body`, which is what makes a
/// title match outrank a body match for the same term; `rank` is bm25's own
/// (negative, smaller-is-better) score, so ORDER BY rank ASC is most-relevant
/// first.
fn run_search(
    conn: &Connection,
    match_expr: &str,
    types: &[String],
    limit: usize,
    offset: usize,
) -> rusqlite::Result<(Vec<Value>, usize, bool)> {
    let type_filter = if types.is_empty() {
        String::new()
    } else {
        let list = types
            .iter()
            .map(|t| format!("'{}'", t.replace('\'', "''")))
            .collect::<Vec<_>>()
            .join(",");
        format!(" AND d.entity_type IN ({list})")
    };

    let sql = format!(
        "SELECT d.doc_id, d.entity_type, d.entity_id, d.title, d.scope, d.task_id,
                d.worker_id, d.link, d.meta, d.updated_at,
                snippet(search_fts, -1, char(1), char(2), '…', 14) AS snip,
                bm25(search_fts, 10.0, 1.0) AS rank
         FROM search_fts
         JOIN search_docs d ON d.rowid_ = search_fts.rowid
         WHERE search_fts MATCH ?1{type_filter}
         ORDER BY rank ASC
         LIMIT ?2 OFFSET ?3"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(
        rusqlite::params![match_expr, limit as i64, offset as i64],
        |r| {
            let meta_raw: String = r.get(8)?;
            let snip: String = r.get(10)?;
            Ok(json!({
                "doc_id":      r.get::<_, String>(0)?,
                "type":        r.get::<_, String>(1)?,
                "id":          r.get::<_, String>(2)?,
                "title":       r.get::<_, String>(3)?,
                "scope":       r.get::<_, Option<String>>(4)?,
                "task_id":     r.get::<_, Option<String>>(5)?,
                "worker_id":   r.get::<_, Option<String>>(6)?,
                "link":        r.get::<_, String>(7)?,
                "meta":        serde_json::from_str::<Value>(&meta_raw).unwrap_or(Value::Null),
                "updated_at":  r.get::<_, i64>(9)?,
                "snippet":     render_snippet(&snip),
                "rank":        r.get::<_, f64>(11)?,
            }))
        },
    )?;
    let hits: Vec<Value> = rows.collect::<rusqlite::Result<_>>()?;

    // Total, capped. `total_capped` is what stops the number being read as
    // exact when it is a floor (Invariant 40: an omission announces itself).
    //
    // With no type filter the join is dead weight — every FTS row has exactly
    // one `search_docs` row by construction, so joining to it cannot change a
    // COUNT. Measured on the real 5,021-doc board index: a common term
    // ("gate", >1000 matches) went 52ms -> 12ms by dropping the join, which is
    // the difference between missing and meeting the plan's <50ms target.
    let count_sql = if types.is_empty() {
        "SELECT COUNT(*) FROM (SELECT 1 FROM search_fts WHERE search_fts MATCH ?1 LIMIT ?2)".to_string()
    } else {
        format!(
            "SELECT COUNT(*) FROM (
                SELECT 1 FROM search_fts
                JOIN search_docs d ON d.rowid_ = search_fts.rowid
                WHERE search_fts MATCH ?1{type_filter} LIMIT ?2)"
        )
    };
    let total: i64 = conn.query_row(
        &count_sql,
        rusqlite::params![match_expr, (COUNT_CAP + 1) as i64],
        |r| r.get(0),
    )?;
    let capped = total as usize > COUNT_CAP;
    Ok((hits, (total as usize).min(COUNT_CAP), capped))
}

// ---------------------------------------------------------------------------
// GET /api/search/status — the drift detector
// ---------------------------------------------------------------------------

/// Per-family: rows in the index vs rows in the live source table, and whether
/// they agree. This is the only thing that distinguishes "search found nothing
/// because nothing matches" from "search found nothing because the index is
/// not being maintained" — and those two look identical from a query alone.
pub fn index_status(conn: &Connection) -> rusqlite::Result<Value> {
    let mut families = Vec::new();
    let mut consistent = true;
    for (etype, table, pred) in FAMILIES {
        let indexed: i64 = conn.query_row(
            "SELECT COUNT(*) FROM search_docs WHERE entity_type = ?1",
            [etype],
            |r| r.get(0),
        )?;
        // A source table can legitimately be absent on a partially-migrated
        // file; report that instead of failing the whole status call.
        let live: Option<i64> = conn
            .query_row(&format!("SELECT COUNT(*) FROM {table} WHERE {pred}"), [], |r| r.get(0))
            .ok();
        let agrees = live.map(|l| l == indexed).unwrap_or(false);
        if !agrees {
            consistent = false;
        }
        families.push(json!({
            "type": etype,
            "table": table,
            "predicate": pred,
            "indexed": indexed,
            "live": live,
            "consistent": agrees,
        }));
    }
    let total_docs: i64 = conn.query_row("SELECT COUNT(*) FROM search_docs", [], |r| r.get(0))?;
    // The FTS side is checked separately: search_docs can be right while the
    // FTS index behind it is empty (a failed trigger, a restored file), and
    // that failure is invisible from doc counts alone.
    let fts_rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM search_fts", [], |r| r.get(0))
        .unwrap_or(-1);
    if fts_rows != total_docs {
        consistent = false;
    }
    Ok(json!({
        "consistent": consistent,
        "docs_total": total_docs,
        "fts_rows": fts_rows,
        "families": families,
    }))
}

async fn status(State(st): State<AppState>) -> Response {
    let conn = match st.store.read() {
        Ok(c) => c,
        Err(e) => return internal(e),
    };
    match index_status(&conn) {
        Ok(v) => Json(v).into_response(),
        Err(e) => internal(e),
    }
}

// ---------------------------------------------------------------------------
// POST /api/search/reindex — the repair, with its counts
// ---------------------------------------------------------------------------

/// Rebuild `search_docs` (and, through its triggers, the FTS index) from the
/// live tables. Same SELECTs as the migration backfill, kept adjacent to it so
/// a schema change has one obvious second place to look.
pub const BACKFILL_SQL: &[(&str, &str)] = &[
    (
        "task",
        "INSERT OR IGNORE INTO search_docs (doc_id, entity_type, entity_id, title, body, scope, task_id, worker_id, link, meta, updated_at)
         SELECT 'task:'||id, 'task', id, title, desc || char(10) || COALESCE(log,''),
                session, id, NULL, '#board/'||id,
                json_object('status', status, 'archived', archived, 'type', type, 'session', session),
                updated
         FROM issues WHERE deleted IS NULL",
    ),
    (
        "schedule",
        "INSERT OR IGNORE INTO search_docs (doc_id, entity_type, entity_id, title, body, scope, task_id, worker_id, link, meta, updated_at)
         SELECT 'schedule:'||id, 'schedule', id, title,
                command || char(10) || COALESCE(schedule_expr,'') || ' ' || COALESCE(recurrence,''),
                session, NULL, NULL, '#schedules',
                json_object('enabled', enabled, 'kind', kind, 'session', session),
                updated
         FROM schedules WHERE deleted IS NULL",
    ),
    (
        "journal",
        "INSERT OR IGNORE INTO search_docs (doc_id, entity_type, entity_id, title, body, scope, task_id, worker_id, link, meta, updated_at)
         SELECT 'journal:'||id, 'journal', id,
                date || CASE WHEN place_name <> '' THEN ' · '||place_name ELSE '' END,
                text || char(10) || tags || ' ' || place_name,
                NULL, NULL, NULL, '#journal',
                json_object('date', date, 'starred', starred, 'tags', tags),
                updated
         FROM journal_entries WHERE deleted IS NULL",
    ),
    (
        "memory",
        "INSERT OR IGNORE INTO search_docs (doc_id, entity_type, entity_id, title, body, scope, task_id, worker_id, link, meta, updated_at)
         SELECT 'memory:'||id, 'memory', id, name, content,
                scope, NULL, json_extract(scope, '$.id'), '#memories',
                json_object('memory_type', memory_type, 'version', version),
                COALESCE(CAST(strftime('%s', updated_at) AS INTEGER), 0)
         FROM _amux_memories WHERE deleted_at IS NULL",
    ),
    (
        "message",
        "INSERT OR IGNORE INTO search_docs (doc_id, entity_type, entity_id, title, body, scope, task_id, worker_id, link, meta, updated_at)
         SELECT 'message:'||id, 'message', id, substr(body, 1, 80), body,
                json_extract(from_actor, '$.id'), NULL, json_extract(target, '$.id'), '#messages',
                json_object('thread', thread, 'from', from_actor, 'target', target),
                COALESCE(CAST(strftime('%s', created_at) AS INTEGER), 0)
         FROM _amux_messages",
    ),
    (
        "prompt",
        "INSERT OR IGNORE INTO search_docs (doc_id, entity_type, entity_id, title, body, scope, task_id, worker_id, link, meta, updated_at)
         SELECT 'prompt:'||id, 'prompt', id,
                substr(replace(text, char(10), ' '), 1, 80), text,
                session, card_id, session, '#history/'||id,
                json_object('session', session, 'origin', origin, 'card_id', card_id),
                ts
         FROM cmd_history WHERE type = 'user'",
    ),
    (
        "worker",
        "INSERT OR IGNORE INTO search_docs (doc_id, entity_type, entity_id, title, body, scope, task_id, worker_id, link, meta, updated_at)
         SELECT 'worker:'||id, 'worker', id, display_name,
                display_name || char(10) || cwd || ' ' || provider || ' '
                    || COALESCE(model,'') || ' ' || backend || ' ' || name_aliases,
                group_id, NULL, id, '#workers/'||id,
                json_object('provider', provider, 'backend', backend, 'model', model, 'group_id', group_id),
                COALESCE(CAST(strftime('%s', updated_at) AS INTEGER), 0)
         FROM _amux_workers",
    ),
];

/// Full rebuild: drop every doc, re-run the backfill. Runs on the writer
/// thread (single-writer discipline), and reports what it did per family.
pub fn rebuild(conn: &Connection) -> rusqlite::Result<Value> {
    let before = index_status(conn)?;
    // DELETE (not DROP): the search_docs delete trigger is what removes the
    // matching FTS rows, so the index cannot be left holding orphans.
    conn.execute("DELETE FROM search_docs", [])?;
    let mut per_family = Vec::new();
    for (etype, sql) in BACKFILL_SQL {
        let inserted = conn.execute(sql, [])?;
        per_family.push(json!({ "type": etype, "indexed": inserted }));
    }
    let after = index_status(conn)?;
    Ok(json!({
        "rebuilt": true,
        "per_family": per_family,
        "before": before,
        "after": after,
    }))
}

async fn reindex(State(st): State<AppState>) -> Response {
    // The rebuild report has to come back out of the writer closure, which
    // can only return a WriteOutcome. A slot the closure fills is the cheapest
    // way to keep the counts — the whole point of the endpoint — attached to
    // the transaction that produced them.
    let slot: std::sync::Arc<std::sync::Mutex<Option<Value>>> = Default::default();
    let slot_for_writer = slot.clone();
    let res = st
        .store
        .write_async(move |conn| {
            let report = rebuild(conn)?;
            *slot_for_writer.lock().unwrap() = Some(report);
            Ok(crate::db::WriteOutcome {
                // A reindex changes only DERIVED state. Bumping the global
                // revision would tell every SSE client that entities changed
                // when none did, so it reports itself as a no-op (Invariant
                // 37) — the transaction still commits.
                applied: false,
                events: vec![],
            })
        })
        .await;
    match res {
        Ok(_) => {
            let report = slot.lock().unwrap().take().unwrap_or(Value::Null);
            Json(report).into_response()
        }
        Err(e) => internal(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn match_expr_quotes_everything_and_prefixes_the_last_term() {
        assert_eq!(build_match("hello"), Some("\"hello\"*".into()));
        assert_eq!(
            build_match("hello world"),
            Some("\"hello\" AND \"world\"*".into())
        );
    }

    #[test]
    fn operators_and_punctuation_cannot_leak_into_fts_syntax() {
        // Each of these is an FTS5 syntax error or an unintended operator when
        // passed through raw. All must become literal terms.
        for q in ["foo:", "a-b", "NOT bar", "C++", "*", "(unbalanced"] {
            let m = build_match(q).expect("non-empty query yields a match expr");
            assert!(
                m.starts_with('"'),
                "query {q:?} must be re-emitted as quoted terms, got {m}"
            );
        }
    }

    #[test]
    fn unterminated_quote_is_a_phrase_not_an_error() {
        assert_eq!(build_match("\"hello world"), Some("\"hello world\"".into()));
    }

    #[test]
    fn empty_query_is_none_not_match_everything() {
        assert_eq!(build_match(""), None);
        assert_eq!(build_match("   "), None);
    }

    #[test]
    fn snippet_escapes_markup_before_adding_marks() {
        let raw = format!("a {HL_OPEN}needle{HL_CLOSE} in <script>alert(1)</script>");
        let out = render_snippet(&raw);
        assert!(out.contains("<mark>needle</mark>"));
        assert!(!out.contains("<script>"));
        assert!(out.contains("&lt;script&gt;"));
    }
}
