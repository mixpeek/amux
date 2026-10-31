//! `GET /api/palette?q=&limit=` — the Cmd-K palette's name index for the
//! two resource families too big to ship to every client: board cards and
//! schedules.
//!
//! The palette answers on every keystroke, so it cannot use `/api/search`:
//! that ranks full card text with bm25, and on the live 15.8 GB database a
//! common word ("deploy", 18k matching documents) took 1 to 14 s depending on
//! what the page cache held. A card's name, though, is 2.8 MB for all 28k
//! cards. So this module keeps (id, title, status, worker, archived, updated)
//! in memory and matches names there, in microseconds.
//!
//! Freshness: a request reads a fingerprint (row counts and the newest
//! `updated` of both tables), at most every 2 s. A changed fingerprint
//! rebuilds the index on a blocking thread while the previous index keeps
//! answering, so a hit can be seconds stale but a keystroke never waits. The
//! first request on a database builds synchronously. `index_age_ms`, `stale`
//! and `n_considered` ride on every answer, so an empty list says whether the
//! index existed (ethos rule 4).
//!
//! Workers, groups, projects, pins, connectors and the rest are small and the
//! dashboard already holds or fetches them, so the client matches those.
//!
//! Ranking (Ethan, 2026-10-24: exact names and ids first, then prefix and
//! word matches, then fuzzy; title over body; completed work demoted,
//! archived and discarded hidden unless asked for) is a tier and a boost:
//! `score = tier * 10000 + boost`. The tier is how well the text matched and
//! always wins; the boost (open over done, recency) only orders hits inside
//! one tier. The dashboard ranks its own families on the same scale, so the
//! two lists merge by score. Each hit says which tier it reached (`match`)
//! and, for a body match, carries a `snippet` around the first matched word.

use super::AppState;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use rusqlite::Connection;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

pub fn routes() -> Router<AppState> {
    Router::new().route("/api/palette", axum::routing::get(palette))
}

const DEFAULT_LIMIT: usize = 12;
const MAX_LIMIT: usize = 50;

#[derive(Clone, Debug)]
pub struct Entry {
    pub kind: &'static str,
    pub id: String,
    pub title: String,
    pub status: String,
    pub session: String,
    pub archived: bool,
    pub updated: i64,
    /// The start of the card's description, whitespace collapsed: what a body
    /// match is found in and its snippet cut from.
    pub body: String,
    /// lowercased "id title session", what tokens are matched against
    hay: String,
    title_lc: String,
    id_lc: String,
    body_lc: String,
}

/// How much of a description is indexed. 400 chars of all 28.6k live cards
/// is 10.5 MB (measured 2026-10-24); whole descriptions would be hundreds.
const BODY_CHARS: usize = 400;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Fingerprint {
    cards: i64,
    cards_updated: i64,
    schedules: i64,
    schedules_updated: i64,
    files: i64,
    files_updated: i64,
}

struct Index {
    entries: Arc<Vec<Entry>>,
    fp: Fingerprint,
    built: Instant,
    build_ms: u64,
}

#[derive(Default)]
struct Slot {
    index: Option<Index>,
    rebuilding: bool,
    /// The last fingerprint read, and when: typing fires a request per
    /// keystroke, and the fingerprint's COUNT took 152ms cold on the live
    /// 28k-card table, so it is re-read at most every FINGERPRINT_EVERY.
    checked: Option<(Instant, Fingerprint)>,
}

const FINGERPRINT_EVERY: std::time::Duration = std::time::Duration::from_secs(2);

/// One index per database file (tests open several in one process).
fn slots() -> &'static Mutex<HashMap<usize, Arc<Mutex<Slot>>>> {
    static S: OnceLock<Mutex<HashMap<usize, Arc<Mutex<Slot>>>>> = OnceLock::new();
    S.get_or_init(Default::default)
}

fn fingerprint(conn: &Connection) -> rusqlite::Result<Fingerprint> {
    let (cards, cards_updated) = conn.query_row(
        "SELECT COUNT(*), COALESCE(MAX(updated), 0) FROM issues WHERE deleted IS NULL",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let (schedules, schedules_updated) = conn.query_row(
        "SELECT COUNT(*), COALESCE(MAX(updated), 0) FROM schedules WHERE deleted IS NULL",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let (files, files_updated) = if crate::api::file_visits::exists(conn) {
        conn.query_row("SELECT SUM(visits), CAST(COALESCE(MAX(last_at), 0) AS INTEGER) FROM file_visits", [], |r| {
            Ok((r.get::<_, Option<i64>>(0)?.unwrap_or(0), r.get(1)?))
        })?
    } else {
        (0, 0)
    };
    Ok(Fingerprint {
        cards,
        cards_updated,
        schedules,
        schedules_updated,
        files,
        files_updated,
    })
}

// One row's fields, as the two SELECTs read them.
#[allow(clippy::too_many_arguments)]
fn entry(
    kind: &'static str,
    id: String,
    title: String,
    status: String,
    session: String,
    archived: bool,
    updated: i64,
    desc: &str,
) -> Entry {
    let id_lc = id.to_lowercase();
    let title_lc = title.to_lowercase();
    let hay = format!("{id_lc} {title_lc} {}", session.to_lowercase());
    let body: String = desc
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(BODY_CHARS)
        .collect();
    let body_lc = body.to_lowercase();
    Entry {
        kind,
        id,
        title,
        status,
        session,
        archived,
        updated,
        body,
        hay,
        title_lc,
        id_lc,
        body_lc,
    }
}

pub fn build(conn: &Connection) -> rusqlite::Result<Vec<Entry>> {
    let mut out = Vec::new();
    let mut st = conn.prepare(
        "SELECT id, title, status, COALESCE(session, ''), COALESCE(archived, 0), updated,
                substr(COALESCE(desc, ''), 1, ?1)
         FROM issues WHERE deleted IS NULL",
    )?;
    // Bytes, not chars, in SQL: twice the budget leaves room for multibyte
    // text before entry() trims it to BODY_CHARS characters.
    let rows = st.query_map([BODY_CHARS as i64 * 2], |r| {
        Ok(entry(
            "card",
            r.get(0)?,
            r.get(1)?,
            r.get(2)?,
            r.get(3)?,
            r.get::<_, i64>(4)? != 0,
            r.get(5)?,
            &r.get::<_, String>(6)?,
        ))
    })?;
    for e in rows {
        out.push(e?);
    }
    let mut st = conn.prepare(
        "SELECT id, title, COALESCE(enabled, 1), COALESCE(session, ''), COALESCE(updated, 0),
                substr(COALESCE(command, ''), 1, ?1)
         FROM schedules WHERE deleted IS NULL",
    )?;
    let rows = st.query_map([BODY_CHARS as i64 * 2], |r| {
        let enabled: i64 = r.get(2)?;
        Ok(entry(
            "schedule",
            r.get(0)?,
            r.get(1)?,
            if enabled != 0 {
                "enabled".into()
            } else {
                "paused".into()
            },
            r.get(3)?,
            false,
            r.get(4)?,
            &r.get::<_, String>(5)?,
        ))
    })?;
    for e in rows {
        out.push(e?);
    }
    // Files the owner has opened (api/file_visits.rs): the name is the title,
    // the path is what a body match finds, the visit count rides in `status`.
    for (path, visits, last_at) in crate::api::file_visits::list(conn)? {
        let name = std::path::Path::new(&path).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| path.clone());
        out.push(entry("file", path.clone(), name, visits.to_string(), String::new(), false, last_at, &path));
    }
    Ok(out)
}

/// Archived and discarded cards: out of the answer unless `hidden=1`.
fn is_hidden(e: &Entry) -> bool {
    e.kind == "card" && (e.archived || e.status == "discarded")
}

fn word_start(hay: &str, tok: &str) -> bool {
    hay.match_indices(tok).any(|(i, _)| {
        i == 0
            || !hay[..i]
                .chars()
                .next_back()
                .is_some_and(char::is_alphanumeric)
    })
}

/// The query's characters in order inside `text` ("dplygt" in "deploy gate"):
/// Some(gap count) when they all appear, the smaller the tighter.
pub fn fuzzy(text: &str, q: &str) -> Option<usize> {
    let mut want = q.chars().filter(|c| !c.is_whitespace()).peekable();
    let (mut gaps, mut started, mut run) = (0usize, false, false);
    for c in text.chars() {
        match want.peek() {
            None => break,
            Some(w) if *w == c => {
                if started && !run {
                    gaps += 1;
                }
                started = true;
                run = true;
                want.next();
            }
            Some(_) => run = false,
        }
    }
    want.peek().is_none().then_some(gaps)
}

/// Up to ~90 chars of `body` around its first match of `tok`, on char
/// boundaries, with an ellipsis where it was cut.
fn snippet(body: &str, body_lc: &str, tok: &str) -> String {
    let at = body_lc.find(tok).unwrap_or(0);
    let at_char = body_lc[..at].chars().count();
    // Lowercasing can change a char count ("İ"); then cut the lowercased text.
    let src: Vec<char> = if body.chars().count() == body_lc.chars().count() {
        body.chars().collect()
    } else {
        body_lc.chars().collect()
    };
    let from = at_char.saturating_sub(30);
    let to = (from + 90).min(src.len());
    let mut out: String = src[from..to].iter().collect();
    if from > 0 {
        out.insert(0, '…');
    }
    if to < src.len() {
        out.push('…');
    }
    out
}

pub struct Hit<'a> {
    pub entry: &'a Entry,
    /// 5 exact id or title, 4 id/title prefix or every word at a word start (title or id),
    /// 3 every word in the id/title/worker, 2 a word only in the body, 1 fuzzy.
    pub tier: i64,
    pub boost: i64,
    pub matched: &'static str,
    pub snippet: Option<String>,
}

impl Hit<'_> {
    pub fn score(&self) -> i64 {
        self.tier * 10_000 + self.boost
    }
}

/// Every hit for `q`, best first, and how many matches were left out as
/// archived or discarded (0 when `with_hidden`).
pub fn rank<'a>(
    entries: &'a [Entry],
    q: &str,
    now: i64,
    with_hidden: bool,
) -> (Vec<Hit<'a>>, usize) {
    let q = q.trim().to_lowercase();
    let toks: Vec<&str> = q.split_whitespace().collect();
    if toks.is_empty() {
        return (Vec::new(), 0);
    }
    let compact: String = q.chars().filter(|c| !c.is_whitespace()).collect();
    let mut hidden = 0usize;
    let mut hits: Vec<Hit> = Vec::new();
    for e in entries {
        let in_hay = toks.iter().all(|t| e.hay.contains(t));
        let (tier, matched, snip, gaps) = if e.id_lc == q || e.title_lc == q {
            (5, "exact", None, 0)
        } else if in_hay
            && (e.id_lc.starts_with(&q)
                || e.title_lc.starts_with(&q)
                || toks
                    .iter()
                    .all(|t| word_start(&e.title_lc, t) || word_start(&e.id_lc, t)))
        {
            (
                4,
                if e.title_lc.starts_with(&q) || e.id_lc.starts_with(&q) {
                    "prefix"
                } else {
                    "word"
                },
                None,
                0,
            )
        } else if in_hay {
            (3, "title", None, 0)
        } else if toks
            .iter()
            .all(|t| e.hay.contains(t) || e.body_lc.contains(t))
        {
            let first = toks
                .iter()
                .find(|t| !e.hay.contains(**t))
                .copied()
                .unwrap_or(toks[0]);
            (2, "body", Some(snippet(&e.body, &e.body_lc, first)), 0)
        } else if compact.chars().count() >= 3 {
            match fuzzy(&e.title_lc, &compact) {
                Some(g) => (1, "fuzzy", None, g),
                None => continue,
            }
        } else {
            continue;
        };
        // A file matches by its name or path, never by a fuzzy guess: long
        // file names fuzzy-match almost any query, and on 2026-10-31 "to-do"
        // put three unrelated docs above real cards.
        if e.kind == "file" && tier < 2 {
            continue;
        }
        if is_hidden(e) && !with_hidden {
            hidden += 1;
            continue;
        }
        let mut b: i64 = match e.kind { "card" => 60, "file" => 70, _ => 50 };
        if matched == "prefix" {
            b += 60;
        }
        if e.kind == "card" {
            b += match e.status.as_str() {
                "doing" | "active" | "review" => 80,
                "done" | "verified" => -300,
                "discarded" => -400,
                _ => 40,
            };
            if e.archived {
                b -= 400;
            }
        } else if e.kind == "file" {
            // Opened often ranks higher: +15 a visit, up to +150.
            b += (e.status.parse::<i64>().unwrap_or(0) * 15).min(150);
        } else if e.status == "enabled" {
            b += 30;
        } else {
            b -= 100;
        }
        b -= (gaps as i64 * 40).min(400);
        // Up to +60 for something touched today, fading over a month.
        let age_days = ((now - e.updated).max(0) as f64) / 86_400.0;
        b += (60.0 * (-age_days / 30.0).exp()) as i64;
        hits.push(Hit {
            entry: e,
            tier,
            boost: b,
            matched,
            snippet: snip,
        });
    }
    hits.sort_by(|a, b| {
        b.score()
            .cmp(&a.score())
            .then(b.entry.updated.cmp(&a.entry.updated))
    });
    (hits, hidden)
}

#[derive(Deserialize)]
pub struct PaletteParams {
    #[serde(default)]
    q: String,
    limit: Option<usize>,
    /// 1: include archived and discarded cards (ranked last in each tier).
    #[serde(default)]
    hidden: u8,
}

async fn palette(State(st): State<AppState>, Query(p): Query<PaletteParams>) -> Response {
    let started = Instant::now();
    let limit = p.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let key = Arc::as_ptr(&st.store) as usize;
    let slot = slots().lock().unwrap().entry(key).or_default().clone();

    let store = st.store.clone();
    let recent = slot
        .lock()
        .unwrap()
        .checked
        .filter(|(at, _)| at.elapsed() < FINGERPRINT_EVERY);
    // How old the "has anything changed" check behind this answer is: within
    // that window a new card can be missing while `stale` still reads false.
    let fp_age_ms = recent
        .map(|(at, _)| at.elapsed().as_millis() as u64)
        .unwrap_or(0);
    let fp = if let Some((_, f)) = recent {
        f
    } else {
        let conn = match store.read() {
            Ok(c) => c,
            Err(e) => return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"error": e.to_string(), "measured": false, "why_unmeasured": "store unavailable"}))).into_response(),
        };
        match fingerprint(&conn) {
            Ok(f) => {
                slot.lock().unwrap().checked = Some((Instant::now(), f));
                f
            }
            Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string(), "measured": false, "why_unmeasured": "fingerprint query failed"}))).into_response(),
        }
    };

    let have_index = slot.lock().unwrap().index.is_some();
    if !have_index {
        // First request on this database: build now.
        let s2 = store.clone();
        let built = tokio::task::spawn_blocking(move || -> anyhow::Result<(Vec<Entry>, u64)> {
            let t0 = Instant::now();
            let conn = s2.read()?;
            let e = build(&conn)?;
            Ok((e, t0.elapsed().as_millis() as u64))
        })
        .await;
        match built {
            Ok(Ok((entries, ms))) => {
                tracing::info!(target: "amux::palette", entries = entries.len(), build_ms = ms, "palette index built");
                slot.lock().unwrap().index = Some(Index { entries: Arc::new(entries), fp, built: Instant::now(), build_ms: ms });
            }
            Ok(Err(e)) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string(), "measured": false, "why_unmeasured": "index build failed"}))).into_response(),
            Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string(), "measured": false, "why_unmeasured": "index build panicked"}))).into_response(),
        }
    }

    // Serve the index we have; refresh it behind this answer if it drifted.
    let (entries, stale, age_ms, build_ms) = {
        let mut g = slot.lock().unwrap();
        let idx = g.index.as_ref().expect("built above");
        let stale = idx.fp != fp;
        let out = (
            idx.entries.clone(),
            stale,
            idx.built.elapsed().as_millis() as u64,
            idx.build_ms,
        );
        if stale && !g.rebuilding {
            g.rebuilding = true;
            let (slot2, s2) = (slot.clone(), store.clone());
            tokio::task::spawn_blocking(move || {
                let t0 = Instant::now();
                let res = s2.read().map_err(|e| e.to_string()).and_then(|c| {
                    let f = fingerprint(&c).map_err(|e| e.to_string())?;
                    build(&c).map(|e| (e, f)).map_err(|e| e.to_string())
                });
                let mut g = slot2.lock().unwrap();
                g.rebuilding = false;
                match res {
                    Ok((entries, f)) => {
                        let ms = t0.elapsed().as_millis() as u64;
                        g.index = Some(Index {
                            entries: Arc::new(entries),
                            fp: f,
                            built: Instant::now(),
                            build_ms: ms,
                        });
                    }
                    Err(e) => {
                        tracing::warn!(target: "amux::palette", error = %e, "verdict=palette_rebuild_failed (serving the previous index)")
                    }
                }
            });
        }
        out
    };

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let (ranked, hidden) = rank(&entries, &p.q, now, p.hidden != 0);
    let total = ranked.len();
    let hits: Vec<Value> = ranked
        .iter()
        .take(limit)
        .map(|h| {
            let e = h.entry;
            json!({
                "kind": e.kind, "id": e.id, "title": e.title, "status": e.status,
                "session": e.session, "archived": e.archived, "updated": e.updated,
                "score": h.score(), "tier": h.tier, "boost": h.boost,
                "match": h.matched, "snippet": h.snippet,
            })
        })
        .collect();
    Json(json!({
        "q": p.q,
        "hits": hits,
        "total": total,
        "hidden": hidden,
        "with_hidden": p.hidden != 0,
        "limit": limit,
        "measured": true,
        "n_considered": entries.len(),
        "stale": stale,
        "fingerprint_age_ms": fp_age_ms,
        "index_age_ms": age_ms,
        "index_build_ms": build_ms,
        "took_ms": started.elapsed().as_millis() as u64,
    }))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_790_000_000;

    /// Ethan, 2026-10-30: typing "to-do" did not find his Mixpeek To-Do.md,
    /// a file he opens all the time.
    #[test]
    fn a_visited_file_is_found_by_its_name_and_the_one_opened_more_ranks_first() {
        let dir = tempfile::tempdir().unwrap();
        let (todo, other) = (dir.path().join("To-Do.md"), dir.path().join("to-do-old.md"));
        std::fs::write(&todo, "x").unwrap();
        std::fs::write(&other, "x").unwrap();
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE issues (id TEXT, title TEXT, status TEXT, session TEXT, archived INT, updated INT, desc TEXT, deleted INT);
             CREATE TABLE schedules (id TEXT, title TEXT, enabled INT, session TEXT, updated INT, command TEXT, deleted INT);").unwrap();
        for _ in 0..9 {
            crate::api::file_visits::record(&conn, &todo.to_string_lossy(), NOW as f64).unwrap();
        }
        crate::api::file_visits::record(&conn, &other.to_string_lossy(), NOW as f64).unwrap();
        let entries = build(&conn).unwrap();
        let (hits, _) = rank(&entries, "to-do", NOW, false);
        let ids: Vec<&str> = hits.iter().map(|h| h.entry.id.as_str()).collect();
        assert_eq!(ids.first().copied(), Some(todo.to_str().unwrap()), "{ids:?}");
        assert_eq!(hits[0].entry.kind, "file");
        assert_eq!(hits[0].entry.title, "To-Do.md");
        assert!(fingerprint(&conn).unwrap().files >= 10, "a visit changes the fingerprint, so the index rebuilds");
        let (fuzzy, _) = rank(&entries, "tdmd", NOW, false);
        assert!(fuzzy.iter().all(|h| h.entry.kind != "file"), "a file is never a fuzzy guess");
    }

    fn e(
        kind: &'static str,
        id: &str,
        title: &str,
        status: &str,
        archived: bool,
        updated: i64,
    ) -> Entry {
        entry(
            kind,
            id.into(),
            title.into(),
            status.into(),
            "lane".into(),
            archived,
            updated,
            "",
        )
    }

    fn ids(hits: &[Hit]) -> Vec<String> {
        hits.iter().map(|h| h.entry.id.clone()).collect()
    }

    #[test]
    fn every_token_must_match_and_ids_win() {
        let v = vec![
            e("card", "AMUX-12", "Fix the deploy gate", "todo", false, NOW),
            e(
                "card",
                "MG-9",
                "Deploy notes for amux",
                "done",
                false,
                NOW - 86_400 * 90,
            ),
            e("card", "AMUX-120", "Unrelated", "todo", false, NOW),
            e(
                "schedule",
                "SCHED-1",
                "nightly deploy check",
                "enabled",
                false,
                NOW,
            ),
        ];
        let (hits, _) = rank(&v, "deploy amux", NOW, false);
        assert_eq!(
            ids(&hits),
            vec!["AMUX-12", "MG-9"],
            "only the two with both words, open first"
        );
        let (hits, _) = rank(&v, "amux-12", NOW, false);
        assert_eq!(
            ids(&hits)[..2],
            ["AMUX-12", "AMUX-120"],
            "exact id first, before the prefix"
        );
        assert_eq!(hits[0].matched, "exact");
        let (hits, _) = rank(&v, "nightly", NOW, false);
        assert_eq!(hits[0].entry.kind, "schedule");
        assert_eq!(
            rank(&v, "   ", NOW, false).0.len(),
            0,
            "blank query matches nothing"
        );
    }

    #[test]
    fn tiers_outrank_status_and_status_orders_a_tier() {
        let v = vec![
            // A word-start match on a fresh open card...
            e("card", "R-1", "Market research for Q4", "doing", false, NOW),
            // ...beats a mid-word match, and a done title match sinks under it.
            e("card", "R-2", "Preresearch notes", "todo", false, NOW),
            e(
                "card",
                "R-3",
                "Research spike",
                "done",
                false,
                NOW - 86_400 * 120,
            ),
            e("card", "R-4", "research", "done", false, NOW - 86_400 * 200),
        ];
        let (hits, _) = rank(&v, "research", NOW, false);
        assert_eq!(ids(&hits), vec!["R-4", "R-1", "R-3", "R-2"],
            "exact name first even when done; then the word tier with the open card above the old done one; mid-word last");
        assert_eq!(
            hits.iter().map(|h| h.tier).collect::<Vec<_>>(),
            vec![5, 4, 4, 3]
        );
    }

    #[test]
    fn archived_and_discarded_are_hidden_and_counted() {
        let v = vec![
            e("card", "A-1", "billing fix", "todo", true, NOW),
            e("card", "A-2", "billing fix", "todo", false, NOW),
            e("card", "A-3", "billing fix", "discarded", false, NOW),
        ];
        let (hits, hidden) = rank(&v, "billing", NOW, false);
        assert_eq!((ids(&hits), hidden), (vec!["A-2".to_string()], 2));
        let (hits, hidden) = rank(&v, "billing", NOW, true);
        assert_eq!(hidden, 0);
        assert_eq!(
            hits[0].entry.id, "A-2",
            "shown on request, still below the live card"
        );
        assert_eq!(hits.len(), 3);
    }

    #[test]
    fn body_matches_rank_under_title_matches_and_carry_a_snippet() {
        let mut body = entry("card", "B-1".into(), "Quarterly cleanup".into(), "todo".into(), "lane".into(), false, NOW,
            "Steps:\n\n  first drain the  parsnip queue, then rotate keys and archive the old bucket");
        body.updated = NOW;
        let v = vec![
            body,
            e(
                "card",
                "B-2",
                "Parsnip rollout",
                "done",
                false,
                NOW - 86_400 * 300,
            ),
        ];
        let (hits, _) = rank(&v, "parsnip", NOW, false);
        assert_eq!(
            ids(&hits),
            vec!["B-2", "B-1"],
            "a done title match still beats an open body match"
        );
        assert_eq!(hits[1].matched, "body");
        let sn = hits[1].snippet.as_deref().unwrap();
        assert!(
            sn.contains("drain the parsnip queue"),
            "whitespace collapsed, around the word: {sn}"
        );
        assert!(hits[0].snippet.is_none());
    }

    #[test]
    fn fuzzy_matches_come_last_and_tight_ones_first() {
        let v = vec![
            e("card", "F-1", "deploy gate", "todo", false, NOW),
            e(
                "card",
                "F-2",
                "dashboard polish lag gate",
                "todo",
                false,
                NOW,
            ),
            e("card", "F-3", "zebra", "todo", false, NOW),
        ];
        let (hits, _) = rank(&v, "dplgate", NOW, false);
        assert_eq!(ids(&hits), vec!["F-1", "F-2"]);
        assert!(hits.iter().all(|h| h.matched == "fuzzy"));
        assert_eq!(fuzzy("deploy", "dpy"), Some(2));
        assert_eq!(fuzzy("deploy", "yd"), None);
        assert!(
            rank(&v, "dp", NOW, false).0.is_empty(),
            "under three letters, no fuzzy guesses"
        );
    }
}
