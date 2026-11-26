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
    /// Scope every family to one session (a card's session, a message's
    /// session, a log's session).
    #[serde(default)]
    session: Option<String>,
}

/// Families searched only when asked for by name in `types=`. They are logs:
/// every message sent to a worker, terminal scrollback and transcripts. Mixed
/// into a default search they would bury the curated hits (AF-499's reason for
/// keeping machine traffic out of `prompt`), so a default search is unchanged.
pub const OPT_IN_TYPES: &[&str] = &["history", "log", "transcript"];

#[derive(Deserialize, Default)]
pub struct ReindexParams {
    /// `firehose=1` also deletes the log/transcript index; the ingest job
    /// rebuilds it from the files.
    #[serde(default)]
    firehose: Option<u8>,
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

/// A message-log query, split into what FTS5 can answer and what it cannot.
///
/// FTS5 matches WORDS: `browser routing` finds a message with both words in any
/// order, which the old `LIKE '%browser routing%'` never did. But the tokenizer
/// throws punctuation away, so a typed `100%` would also match a bare `100`.
/// Any term that carries punctuation is therefore ALSO required literally, as a
/// substring, which keeps `q=100%` meaning exactly what the typist wrote.
///
/// Returns (MATCH expression if the query has any word, literal substrings).
/// Both empty means nothing searchable: the caller falls back to a plain
/// substring match on the whole query.
pub fn history_query(q: &str) -> (Option<String>, Vec<String>) {
    let has_word = q.chars().any(char::is_alphanumeric);
    let fts = if has_word { build_match(q) } else { None };
    let literals = q
        .split_whitespace()
        .map(|t| t.trim_matches('"'))
        .filter(|t| !t.is_empty() && t.chars().any(|c| !c.is_alphanumeric()))
        .map(str::to_string)
        .collect();
    (fts, literals)
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

/// The entity types the index actually holds, ascending. Read from the DATA,
/// not restated from the six INSERT sites below, so a newly-indexed family
/// cannot make this list quietly wrong.
///
/// AF-547. `types=` filters `d.entity_type IN (...)`, so a value the index does
/// not hold returns a clean 200 with zero hits — indistinguishable from "your
/// query matched nothing". A caller filtering on a name they carried from
/// another endpoint gets a confident empty answer, which on the approval-
/// verification path reads as "no approval exists".
///
/// Phrased as NOT PRESENT rather than INVALID on purpose: a family with no rows
/// yet (no journals written, say) is absent without the caller being wrong, and
/// this cannot tell those apart. It reports what it can prove.
fn types_in_index(conn: &Connection) -> Vec<String> {
    conn.prepare("SELECT DISTINCT entity_type FROM search_docs ORDER BY 1")
        .and_then(|mut st| {
            st.query_map([], |r| r.get::<_, String>(0))
                .map(|it| it.flatten().collect())
        })
        .unwrap_or_default()
}

/// Which of `requested` the index does not hold. Extracted so the TEST CALLS
/// THE SHIPPED CODE instead of recomputing the same expression beside it.
///
/// The first version of that test did recompute it, and a mutation that made
/// the handler's filter never fire left the suite GREEN — the test could not
/// see a change to the thing it was testing. ts-gke had described that exact
/// shape an hour earlier, about the bug this card came from: "my control shared
/// the defect with the measurement ... a control built from the same mistaken
/// assumption as the measurement cannot fail."
fn types_missing_from(requested: &[String], have: &[String]) -> Vec<String> {
    requested
        .iter()
        .filter(|t| !have.contains(t))
        .cloned()
        .collect()
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
    let all_types = parse_types(&p.types);
    let opt_in: Vec<String> = all_types
        .iter()
        .filter(|t| OPT_IN_TYPES.contains(&t.as_str()))
        .cloned()
        .collect();
    let types: Vec<String> = all_types
        .iter()
        .filter(|t| !OPT_IN_TYPES.contains(&t.as_str()))
        .cloned()
        .collect();
    let session = p
        .session
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    let Some(match_expr) = build_match(&p.q) else {
        return Json(json!({
            "q": p.q,
            "types": all_types,
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
    // Only paid for when the caller actually filtered — a search with no
    // `types=` cannot have an unknown one, and charging every request a DISTINCT
    // for a question nobody asked is the arithmetic ethos rule 2 forbids.
    let (available, not_in_index): (Vec<String>, Vec<String>) = if types.is_empty() {
        (Vec::new(), Vec::new())
    } else {
        let have = types_in_index(&conn);
        let missing = types_missing_from(&types, &have);
        (have, missing)
    };
    if !not_in_index.is_empty() {
        tracing::warn!(
            target: "amux::search",
            requested = ?types, not_in_index = ?not_in_index, available = ?available,
            "search filtered on entity type(s) the index does not hold — the zero is the filter, not the data"
        );
    }

    if !opt_in.is_empty() {
        return search_merged(
            &conn,
            &p.q,
            &match_expr,
            &all_types,
            &types,
            &opt_in,
            session.as_deref(),
            limit,
            limit_raw,
            offset,
            started,
            not_in_index,
            available,
        );
    }

    match run_search(
        &conn,
        &match_expr,
        &types,
        session.as_deref(),
        limit,
        offset,
    ) {
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
            let indexed = if hits.is_empty() {
                index_doc_count(&conn)
            } else {
                None
            };
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
                "types": all_types,
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
                // AF-547: named BESIDE the answer, so a zero caused by the
                // FILTER is distinguishable from a zero caused by the DATA.
                // Empty on every ordinary query; non-empty only when the caller
                // asked for a family the index does not hold, which is the one
                // case where the count means nothing.
                "types_not_in_index": not_in_index,
                "types_available": available,
            }))
            .into_response()
        }
        Err(e) => internal(e),
    }
}

/// Ranked message-log hits (`types=history`), in the shared hit shape.
fn run_history(
    conn: &Connection,
    match_expr: &str,
    session: Option<&str>,
    limit: usize,
) -> rusqlite::Result<(Vec<Value>, usize, bool)> {
    let sess = if session.is_some() {
        " AND h.session = ?3"
    } else {
        ""
    };
    let sql = format!(
        "SELECT h.id, h.session, h.type, h.ts, h.text, h.card_id,
                snippet(history_fts, 0, char(1), char(2), '…', 16), bm25(history_fts) AS rank
         FROM history_fts JOIN cmd_history h ON h.id = history_fts.rowid
         WHERE history_fts MATCH ?1{sess}
         ORDER BY rank LIMIT ?2"
    );
    let mut st = conn.prepare(&sql)?;
    let map = |r: &rusqlite::Row| -> rusqlite::Result<Value> {
        let id: i64 = r.get(0)?;
        let session: String = r.get(1)?;
        let text: String = r.get(4)?;
        Ok(json!({
            "doc_id": format!("history:{id}"),
            "type": "history",
            "id": format!("MSG-{id}"),
            "title": text.replace('\n', " ").chars().take(80).collect::<String>(),
            "scope": session,
            "task_id": r.get::<_, Option<String>>(5)?,
            "worker_id": session,
            "link": format!("#history/{id}"),
            "meta": {"session": session, "type": r.get::<_, String>(2)?},
            "updated_at": r.get::<_, i64>(3)?,
            "snippet": r.get::<_, String>(6)?,
            "rank": r.get::<_, f64>(7)?,
        }))
    };
    let hits: Vec<Value> = match session {
        Some(s) => st
            .query_map(rusqlite::params![match_expr, limit as i64, s], map)?
            .collect::<rusqlite::Result<_>>()?,
        None => st
            .query_map(rusqlite::params![match_expr, limit as i64], map)?
            .collect::<rusqlite::Result<_>>()?,
    };
    let count_sql = format!(
        "SELECT COUNT(*) FROM (SELECT 1 FROM history_fts JOIN cmd_history h ON h.id = history_fts.rowid
         WHERE history_fts MATCH ?1{sess} LIMIT ?2)"
    );
    let cap = (COUNT_CAP + 1) as i64;
    let total: i64 = match session {
        Some(s) => conn.query_row(&count_sql, rusqlite::params![match_expr, cap, s], |r| {
            r.get(0)
        })?,
        None => conn.query_row(&count_sql, rusqlite::params![match_expr, cap], |r| r.get(0))?,
    };
    Ok((
        hits,
        (total as usize).min(COUNT_CAP),
        total as usize > COUNT_CAP,
    ))
}

/// Order hits from several indexes into one list: bm25 ascending (FTS5's rank
/// is negative, more relevant is smaller), then the page window. Pure, so the
/// merge has cells without three databases.
pub fn merge_ranked(mut lists: Vec<Vec<Value>>, limit: usize, offset: usize) -> Vec<Value> {
    let mut all: Vec<Value> = lists.drain(..).flatten().collect();
    all.sort_by(|a, b| {
        let ra = a["rank"].as_f64().unwrap_or(0.0);
        let rb = b["rank"].as_f64().unwrap_or(0.0);
        ra.partial_cmp(&rb).unwrap_or(std::cmp::Ordering::Equal)
    });
    all.into_iter().skip(offset).take(limit).collect()
}

/// A search that names an opt-in family: each index is asked for its best
/// `offset + limit`, and the merged list is windowed once.
#[allow(clippy::too_many_arguments)]
fn search_merged(
    conn: &Connection,
    q: &str,
    match_expr: &str,
    all_types: &[String],
    curated: &[String],
    opt_in: &[String],
    session: Option<&str>,
    limit: usize,
    limit_raw: usize,
    offset: usize,
    started: std::time::Instant,
    not_in_index: Vec<String>,
    available: Vec<String>,
) -> Response {
    let want = offset + limit;
    let mut lists = Vec::new();
    let mut total = 0usize;
    let mut capped = false;
    let mut add =
        |r: rusqlite::Result<(Vec<Value>, usize, bool)>, render: bool| -> rusqlite::Result<()> {
            let (mut hits, t, c) = r?;
            if render {
                for h in hits.iter_mut() {
                    let raw = h["snippet"].as_str().unwrap_or("").to_string();
                    h["snippet"] = json!(render_snippet(&raw));
                }
            }
            lists.push(hits);
            total += t;
            capped |= c;
            Ok(())
        };
    let home = crate::config::amux_home();
    let firehose_kinds: Vec<&str> = opt_in
        .iter()
        .map(String::as_str)
        .filter(|t| *t == "log" || *t == "transcript")
        .collect();
    let res = (|| -> rusqlite::Result<()> {
        if !curated.is_empty() {
            add(
                run_search(conn, match_expr, curated, session, want, 0),
                false,
            )?;
        }
        if opt_in.iter().any(|t| t == "history") {
            add(run_history(conn, match_expr, session, want), true)?;
        }
        if !firehose_kinds.is_empty() {
            add(
                crate::search_firehose::search(
                    &home,
                    match_expr,
                    q,
                    &firehose_kinds,
                    session,
                    want,
                    COUNT_CAP,
                ),
                true,
            )?;
        }
        Ok(())
    })();
    if let Err(e) = res {
        return internal(e);
    }
    let hits = merge_ranked(lists, limit, offset);
    // A log/transcript search answered while the index is still being built
    // can miss hits, and a zero from it must not read as "not there"
    // (TG-3303). Name the bytes the index has not reached yet.
    let firehose_lag = (!firehose_kinds.is_empty())
        .then(|| crate::search_firehose::pending_bytes(&home, &firehose_kinds, session));
    Json(json!({
        "index_lag_bytes": firehose_lag,
        "q": q,
        "match": match_expr,
        "types": all_types,
        "session": session,
        "hits": hits,
        "total": total,
        "total_capped": capped,
        "limit": limit,
        "limit_capped": limit_raw > MAX_LIMIT,
        "offset": offset,
        "took_ms": started.elapsed().as_millis() as u64,
        "types_not_in_index": not_in_index,
        "types_available": available,
    }))
    .into_response()
}

/// How many documents the index holds, or `None` if the count could not be
/// taken. `None` is deliberately not `0`: "I could not measure" and "there is
/// nothing there" send a reader to different places.
fn index_doc_count(conn: &Connection) -> Option<i64> {
    conn.query_row("SELECT COUNT(*) FROM search_docs", [], |r| {
        r.get::<_, i64>(0)
    })
    .ok()
}

/// The ranked query. bm25 weights `title` 10x `body`, which is what makes a
/// title match outrank a body match for the same term; `rank` is bm25's own
/// (negative, smaller-is-better) score, so ORDER BY rank ASC is most-relevant
/// first.
fn run_search(
    conn: &Connection,
    match_expr: &str,
    types: &[String],
    session: Option<&str>,
    limit: usize,
    offset: usize,
) -> rusqlite::Result<(Vec<Value>, usize, bool)> {
    let session_filter = match session {
        Some(s) => format!(" AND d.scope = '{}'", s.replace('\'', "''")),
        None => String::new(),
    };
    let type_filter = if types.is_empty() {
        session_filter
    } else {
        let list = types
            .iter()
            .map(|t| format!("'{}'", t.replace('\'', "''")))
            .collect::<Vec<_>>()
            .join(",");
        format!(" AND d.entity_type IN ({list}){session_filter}")
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
    let count_sql = if type_filter.is_empty() {
        "SELECT COUNT(*) FROM (SELECT 1 FROM search_fts WHERE search_fts MATCH ?1 LIMIT ?2)"
            .to_string()
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
            .query_row(
                &format!("SELECT COUNT(*) FROM {table} WHERE {pred}"),
                [],
                |r| r.get(0),
            )
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
    // The message-log index (0119) is external content over cmd_history, so
    // its own row count is the docsize table, one row per indexed message.
    let h_indexed: Option<i64> = conn
        .query_row("SELECT COUNT(*) FROM history_fts_docsize", [], |r| r.get(0))
        .ok();
    let h_live: Option<i64> = conn
        .query_row("SELECT COUNT(*) FROM cmd_history", [], |r| r.get(0))
        .ok();
    let h_agrees = h_indexed.is_some() && h_indexed == h_live;
    if !h_agrees {
        consistent = false;
    }
    families.push(json!({
        "type": "history",
        "table": "cmd_history",
        "predicate": "1=1",
        "indexed": h_indexed,
        "live": h_live,
        "consistent": h_agrees,
        "opt_in": true,
    }));
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
        Ok(mut v) => {
            let home = crate::config::amux_home();
            v["firehose"] =
                tokio::task::spawn_blocking(move || crate::search_firehose::status(&home))
                    .await
                    .unwrap_or(Value::Null);
            v["opt_in_types"] = json!(OPT_IN_TYPES);
            Json(v).into_response()
        }
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
        "prompt",
        "INSERT OR IGNORE INTO search_docs (doc_id, entity_type, entity_id, title, body, scope, task_id, worker_id, link, meta, updated_at)
         SELECT 'prompt:'||id, 'prompt', id,
                substr(replace(text, char(10), ' '), 1, 80), text,
                session, card_id, session, '#history/'||id,
                json_object('session', session, 'origin', origin, 'card_id', card_id),
                -- ts/1000: cmd_history.ts is MILLISECONDS and every other
                -- family here contributes seconds (AMUX-4548). Without the
                -- divide this column holds two units at once, and 0056 put
                -- 1,910 such rows in it. Must stay in step with the
                -- `search_prompt_ai` trigger in migration 0077, or a reindex
                -- and a live insert disagree about the same document.
                ts/1000
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
    conn.execute(
        "INSERT INTO history_fts(history_fts) VALUES ('rebuild')",
        [],
    )?;
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

async fn reindex(State(st): State<AppState>, Query(rp): Query<ReindexParams>) -> Response {
    if rp.firehose == Some(1) {
        if let Err(e) = crate::search_firehose::reset(&crate::config::amux_home()) {
            return internal(e);
        }
    }
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
    fn history_query_splits_words_from_literal_punctuation() {
        let (m, lit) = history_query("browser routing");
        assert_eq!(m.as_deref(), Some("\"browser\" AND \"routing\"*"));
        assert!(lit.is_empty());
        let (m, lit) = history_query("100%");
        assert!(m.is_some());
        assert_eq!(lit, vec!["100%"]);
        let (m, lit) = history_query("%");
        assert_eq!(m, None, "no word: nothing for FTS5");
        assert_eq!(lit, vec!["%"]);
    }

    #[test]
    fn merged_hits_rank_across_indexes_then_window() {
        let a = vec![json!({"id":"a","rank":-1.0}), json!({"id":"c","rank":-9.0})];
        let b = vec![json!({"id":"b","rank":-5.0})];
        let ids = |v: Vec<Value>| {
            v.iter()
                .map(|h| h["id"].as_str().unwrap().to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            ids(merge_ranked(vec![a.clone(), b.clone()], 10, 0)),
            vec!["c", "b", "a"]
        );
        assert_eq!(ids(merge_ranked(vec![a, b], 1, 1)), vec!["b"]);
    }

    /// `types=history` reaches every message through 0119's index, scoped by
    /// session, and a default search still does not (AF-499).
    #[test]
    fn history_family_is_searchable_by_name_only() {
        let conn = crate::db::migrate::test_memdb();
        for (text, session, ty) in [
            ("browser routing relay", "alpha", "session"),
            ("routing the browser again", "beta", "user"),
        ] {
            conn.execute(
                "INSERT INTO cmd_history (text, type, session, ts) VALUES (?1, ?3, ?2, 1)",
                rusqlite::params![text, session, ty],
            )
            .unwrap();
        }
        let m = build_match("browser routing").unwrap();
        let (hits, total, _) = run_history(&conn, &m, None, 10).unwrap();
        assert_eq!(total, 2);
        assert!(hits.iter().all(|h| h["type"] == json!("history")));
        let (hits, ..) = run_history(&conn, &m, Some("alpha"), 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0]["id"], json!("MSG-1"));
        // The status view counts it, and it agrees with the table.
        let st = index_status(&conn).unwrap();
        let fam = st["families"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["type"] == json!("history"))
            .unwrap()
            .clone();
        assert_eq!(fam["indexed"], json!(2));
        assert_eq!(fam["consistent"], json!(true));
    }

    /// 0077's REPAIR CLAUSE CONVERTS THE ROWS 0056 ALREADY WROTE, AND ONLY ONCE
    /// (AMUX-4548).
    ///
    /// Written because the obvious coverage does not reach it. Every other cell
    /// here runs against `test_memdb`, a fresh schema where no millisecond row
    /// has ever existed, so deleting the repair entirely leaves them all green:
    /// the clause is untestable from a fixture that has nothing to repair.
    /// Measured, not assumed — mutating the UPDATE to `SELECT 1` passed 2 of 2
    /// before this cell existed.
    ///
    /// So the fixture writes the damage first, straight into `search_docs` and
    /// past the trigger, which is the shape the live database was actually in:
    /// 1,910 millisecond rows among 21,465.
    ///
    /// The SQL comes from the migration file itself rather than a copy, so this
    /// pins the bytes that ship.
    #[test]
    fn the_migration_repairs_already_written_millisecond_rows_and_is_idempotent() {
        let conn = crate::db::migrate::test_memdb();
        let ms: i64 = 1_789_559_556_000;
        // Past the trigger deliberately: this is a row 0056 left behind, not
        // one a fixed writer could produce.
        conn.execute(
            "INSERT INTO search_docs (doc_id, entity_type, entity_id, title, body, scope, \
             task_id, worker_id, link, meta, updated_at) \
             VALUES ('prompt:legacy', 'prompt', 'legacy', 't', 'b', 'amux', NULL, 'amux', \
             '#history/legacy', '{}', ?1)",
            [ms],
        )
        .expect("seed the damage");
        // A seconds row of the same vintage, as the control: the repair must
        // leave it alone, or it would divide the other six families too.
        conn.execute(
            "INSERT INTO search_docs (doc_id, entity_type, entity_id, title, body, scope, \
             task_id, worker_id, link, meta, updated_at) \
             VALUES ('task:control', 'task', 'control', 't', 'b', 'amux', 'control', NULL, \
             '#board/control', '{}', ?1)",
            [ms / 1000],
        )
        .unwrap();

        let sql = include_str!("../../migrations/0077_search_docs_prompt_seconds.sql");
        conn.execute_batch(sql).expect("0077 applies");
        let after: i64 = conn
            .query_row(
                "SELECT updated_at FROM search_docs WHERE doc_id='prompt:legacy'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            after,
            ms / 1000,
            "the legacy millisecond row must be converted"
        );

        // IDEMPOTENT. The guard is on magnitude, so a second run must not
        // divide again — a migration that re-runs on a repaired database would
        // put these rows in 1970.
        conn.execute_batch(sql).expect("0077 re-applies");
        let twice: i64 = conn
            .query_row(
                "SELECT updated_at FROM search_docs WHERE doc_id='prompt:legacy'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            twice, after,
            "a second run must be a no-op, not a second division"
        );

        let control: i64 = conn
            .query_row(
                "SELECT updated_at FROM search_docs WHERE doc_id='task:control'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            control,
            ms / 1000,
            "a seconds row of another family must be untouched"
        );
    }

    /// `search_docs.updated_at` HOLDS ONE UNIT, AND BOTH PROMPT WRITERS AGREE
    /// (AMUX-4548).
    ///
    /// `cmd_history.ts` is milliseconds; every other source column feeding this
    /// table is seconds. 0056 wired the prompt family straight through, so the
    /// column held two units at once: measured on the live DB 2026-09-16, 1,910
    /// of 21,465 rows were millisecond-shaped and `schema.timestamp_units_
    /// declared` reported the column's MAX as 496,602,776 hours in the past.
    ///
    /// TWO WRITERS, ONE ASSERTION. The trigger is the live path and
    /// `BACKFILL_SQL` is the reindex path, and nothing but a comment keeps them
    /// in step. If they diverge, the same prompt has two different timestamps
    /// depending on whether the index was rebuilt, which is worse than the
    /// original bug and would show up as a puzzling inconsistency rather than
    /// as an obviously wrong number. So this runs BOTH and requires them equal.
    ///
    /// Against the real migration chain, not a hand-made schema: the trigger
    /// under test is created by a migration, and a fixture that builds its own
    /// tables would not have it.
    #[test]
    fn a_prompt_is_indexed_in_seconds_by_the_trigger_and_by_the_backfill() {
        let conn = crate::db::migrate::test_memdb();
        let ms: i64 = 1_789_559_556_000;
        let want: i64 = ms / 1000;
        conn.execute(
            "INSERT INTO cmd_history (id, text, type, session, ts, origin, card_id) \
             VALUES (1, 'find the hubspot thread', 'user', 'amux', ?1, '', NULL)",
            [ms],
        )
        .expect("insert a human prompt");

        // 1. The TRIGGER path, which is what runs in production.
        let via_trigger: i64 = conn
            .query_row(
                "SELECT updated_at FROM search_docs WHERE doc_id='prompt:1'",
                [],
                |r| r.get(0),
            )
            .expect("the trigger indexed it");
        assert_eq!(
            via_trigger, want,
            "the trigger must store SECONDS; {via_trigger} is the raw millisecond value and puts \
             this prompt tens of thousands of years from now"
        );

        // 2. The REINDEX path, over the same row.
        conn.execute("DELETE FROM search_docs WHERE doc_id='prompt:1'", [])
            .unwrap();
        let sql = BACKFILL_SQL
            .iter()
            .find(|(etype, _)| *etype == "prompt")
            .map(|(_, sql)| *sql)
            .expect("the prompt family is in BACKFILL_SQL");
        conn.execute(sql, []).expect("backfill runs");
        let via_backfill: i64 = conn
            .query_row(
                "SELECT updated_at FROM search_docs WHERE doc_id='prompt:1'",
                [],
                |r| r.get(0),
            )
            .expect("the backfill indexed it");
        assert_eq!(
            via_backfill, via_trigger,
            "a reindex and a live insert must agree about the same document"
        );

        // 3. The column-level property the invariant actually checks: nothing
        //    in it is millisecond-shaped. Asserted over the whole table rather
        //    than the one row, because the defect was one family among seven.
        let ms_shaped: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM search_docs WHERE updated_at > 100000000000",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            ms_shaped, 0,
            "no row in search_docs.updated_at may be millisecond-shaped"
        );
    }

    /// AF-547. A `types=` value the index does not hold returns a clean 200 with
    /// zero hits, indistinguishable from "your query matched nothing". On the
    /// approval-verification path that reads as "no approval exists".
    ///
    /// Built from a REAL index rather than a hand-made list, because the bug this
    /// came from was a control that shared its defect with the measurement:
    /// ts-gke counted `kind` values to prove `kind` held no prompts, when the
    /// real question was whether `kind` EXISTED. A control that assumes what it
    /// is testing cannot fail.
    #[test]
    fn a_type_the_index_does_not_hold_is_named_beside_the_zero() {
        let conn = Connection::open_in_memory().expect("mem db");
        conn.execute_batch(
            "CREATE TABLE search_docs (doc_id TEXT, entity_type TEXT, entity_id TEXT,
                 title TEXT, body TEXT, scope TEXT, task_id TEXT, worker_id TEXT,
                 link TEXT, meta TEXT, updated_at INTEGER);
             INSERT INTO search_docs (doc_id, entity_type) VALUES
                 ('task:A-1','task'), ('prompt:1','prompt'), ('prompt:2','prompt');",
        )
        .expect("seed");

        let have = types_in_index(&conn);
        assert_eq!(
            have,
            vec!["prompt".to_string(), "task".to_string()],
            "read from the DATA, sorted"
        );

        // The reported shape: a family name carried from another endpoint.
        let requested = vec!["kind".to_string()];
        let missing = types_missing_from(&requested, &have);
        assert_eq!(
            missing,
            vec!["kind".to_string()],
            "an absent family must be NAMED, not silently empty"
        );

        // CONTROL 1: a real family must NOT be reported missing. Without this the
        // rule is satisfiable by flagging everything, which would put a false
        // "not in index" on every correct query.
        let ok = types_missing_from(&["prompt".to_string()], &have);
        assert!(
            ok.is_empty(),
            "a family the index holds is not missing: {ok:?}"
        );

        // CONTROL 2: mixed — the real one passes, only the bogus one is named.
        let req: Vec<String> = ["prompt", "kind", "task"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let mixed = types_missing_from(&req, &have);
        assert_eq!(
            mixed,
            vec!["kind".to_string()],
            "only the absent one: {mixed:?}"
        );
    }

    /// An EMPTY index must report an empty vocabulary rather than crashing or
    /// inventing one — and then every requested type reads as missing, which is
    /// correct and is what the 503 zero-hit path above is for.
    #[test]
    fn an_empty_index_has_an_empty_vocabulary() {
        let conn = Connection::open_in_memory().expect("mem db");
        conn.execute_batch("CREATE TABLE search_docs (doc_id TEXT, entity_type TEXT);")
            .expect("seed");
        assert!(types_in_index(&conn).is_empty());
        // And a missing TABLE must not panic — the helper is called on every
        // filtered search and an unreadable index is not a crash.
        let bare = Connection::open_in_memory().expect("mem db");
        assert!(
            types_in_index(&bare).is_empty(),
            "no table -> empty, not a panic"
        );
    }

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
