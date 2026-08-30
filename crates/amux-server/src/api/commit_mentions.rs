//! `GET /api/board/commit-mentions` — open cards named in a merged commit.
//!
//! AMUX-2674. Five cards sat in `todo` while ONE commit (e679bdb) fixed all
//! five and named every id in its body. Auto-pickup kept offering finished work,
//! each card costing a full scope-and-decide cycle to rediscover, and anyone
//! reading the board saw five live-in-production defects as open.
//!
//! The data was already there and machine-readable — nothing read it. That is
//! the same shape as AC-323's `ignored_fields`: a correct signal with no
//! consumer. This is the consumer.
//!
//! # It SURFACES, it never closes
//!
//! A card id in a commit is NOT proof of completion. Commits reference cards for
//! context, for partial work, and for reverts. Auto-closing on a mention would
//! silently close open work — ethos rule 8, and precisely what the board's
//! done-vs-verified split exists to prevent. So this endpoint is a GET that
//! mutates nothing and reports candidates for a human or session to judge.
//!
//! # Which cards it looks at, and why not pickup's predicate
//!
//! Ethos rule 1's corollary: a view must share the predicate of the mechanism it
//! claims to describe, or it is worse than no view. This one describes the
//! BOARD'S OPEN SET — "cards that still read as open while a merged commit says
//! otherwise" — so its predicate is openness: `status IN ('todo','doing')`, not
//! deleted, not archived.
//!
//! It deliberately does NOT copy `board_drive`'s pickup predicate
//! (`owner_type='agent'`, no `needs:you` tag, type not tripwire/watch, plus
//! freshness and reclaim windows). Those encode *who may be handed this next*,
//! which is a different question — a human-owned card fixed by a merged commit
//! is exactly as stale, and filtering it out would hide it. If you find yourself
//! "fixing" this to match pickup, that is the deviation; it is intentional and
//! this paragraph is why.

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

use super::AppState;

/// Record separator between commits, unit separator between fields. Chosen
/// because a commit body can contain anything a human types, including newlines
/// and pipes — splitting on those loses bodies, which is where the ids live.
const REC: char = '\u{1e}';
const UNIT: char = '\u{1f}';

/// Cap on commits parsed per repo. A repo with thousands of matching commits is
/// a signal to narrow the query, not something to spend minutes on — and the cap
/// is REPORTED (`truncated`) rather than silently applied, because a silent
/// truncation reads as "covered everything" when it did not.
const MAX_COMMITS: usize = 400;

/// `git log --grep` takes one alternation; a very long one is slow and can blow
/// the arg limit. Ids are chunked and the chunks unioned.
const IDS_PER_QUERY: usize = 120;

#[derive(Deserialize, Default)]
pub struct Params {
    /// Restrict to one session's cards.
    pub session: Option<String>,
}

/// Open cards, keyed by id. Openness is the predicate — see the module docs for
/// why this is not pickup's.
fn open_cards(
    conn: &rusqlite::Connection,
    session: Option<&str>,
) -> rusqlite::Result<BTreeMap<String, Value>> {
    let sql = "SELECT id, status, session, COALESCE(title,''), COALESCE(type,'') \
               FROM issues \
               WHERE status IN ('todo','doing') AND deleted IS NULL \
               AND COALESCE(archived,0)=0 \
               AND (?1 IS NULL OR session = ?1)";
    let mut st = conn.prepare(sql)?;
    let rows = st.query_map(rusqlite::params![session], |r| {
        Ok((
            r.get::<_, String>(0)?,
            json!({
                "status":  r.get::<_, String>(1)?,
                "session": r.get::<_, String>(2)?,
                "title":   r.get::<_, String>(3)?,
                "type":    r.get::<_, String>(4)?,
            }),
        ))
    })?;
    Ok(rows.flatten().collect())
}

/// `git rev-parse --show-toplevel`, so several sessions sharing one checkout
/// collapse to a single scan.
async fn git_toplevel(dir: &str) -> Option<String> {
    let out = tokio::process::Command::new("git")
        .args(["-C", dir, "rev-parse", "--show-toplevel"])
        .output()
        .await
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Word-boundary match for a card id inside a commit message.
///
/// Without the boundary, `AMUX-26` matches `AMUX-2674` and the report names
/// cards no commit mentions — a filter that matches too much returns a
/// confident wrong answer rather than silence (ethos rule 7). `git log --grep`
/// does the cheap narrowing; this does the exact matching.
fn ids_in_text(text: &str, known: &BTreeSet<&str>) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    let bytes = text.as_bytes();
    for id in known {
        let mut from = 0;
        while let Some(rel) = text[from..].find(*id) {
            let start = from + rel;
            let end = start + id.len();
            let before_ok = start == 0 || !bytes[start - 1].is_ascii_alphanumeric();
            // The char AFTER must not be a digit, or AMUX-26 swallows AMUX-2674.
            let after_ok = end >= bytes.len() || !bytes[end].is_ascii_alphanumeric();
            if before_ok && after_ok {
                found.insert((*id).to_string());
                break;
            }
            from = end;
        }
    }
    found
}

/// One `git log` per repo per id-chunk. Returns (sha, subject, ids).
async fn scan_repo(
    repo: &str,
    ids: &BTreeSet<&str>,
) -> (Vec<(String, String, BTreeSet<String>)>, bool) {
    let all: Vec<&str> = ids.iter().copied().collect();
    let mut hits: Vec<(String, String, BTreeSet<String>)> = Vec::new();
    let mut truncated = false;

    for chunk in all.chunks(IDS_PER_QUERY) {
        // Reachable from HEAD, which is the point: a commit on someone's
        // unmerged branch has not fixed anything from this checkout's view.
        let pattern = chunk.join("|");
        let fmt = format!("--format=%H{UNIT}%s{UNIT}%B{REC}");
        let out = tokio::process::Command::new("git")
            .args([
                "-C",
                repo,
                "log",
                "-E",
                "--no-merges",
                &format!("--grep={pattern}"),
                &format!("--max-count={MAX_COMMITS}"),
                &fmt,
            ])
            .output()
            .await;
        let Ok(out) = out else { continue };
        if !out.status.success() {
            continue;
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let mut n = 0;
        for rec in text.split(REC) {
            let rec = rec.trim_start_matches('\n');
            if rec.trim().is_empty() {
                continue;
            }
            n += 1;
            let mut parts = rec.splitn(3, UNIT);
            let (Some(sha), Some(subject), Some(body)) =
                (parts.next(), parts.next(), parts.next())
            else {
                continue;
            };
            let found = ids_in_text(&format!("{subject}\n{body}"), ids);
            if !found.is_empty() {
                hits.push((
                    sha.trim().chars().take(12).collect(),
                    subject.trim().to_string(),
                    found,
                ));
            }
        }
        if n >= MAX_COMMITS {
            truncated = true;
        }
    }
    (hits, truncated)
}

/// The HTTP shell. The body is [`mentions_payload`] so a JOB can ask the same
/// question the endpoint answers, without going through the router (AMUX-3579).
/// The endpoint had zero consumers for months; giving it a callable form is
/// what let it reach anything at all.
pub async fn commit_mentions(
    State(state): State<AppState>,
    Query(p): Query<Params>,
) -> (StatusCode, Json<Value>) {
    let (code, body) = mentions_payload_with(&state, p).await;
    (code, Json(body))
}

/// Default params — what a job wants.
/// Returns an EMPTY payload on error rather than an error body: a job asking
/// this question wants "nothing to note", and the endpoint keeps its 500 so a
/// human caller still learns the store was unreadable. Same computation, two
/// honest presentations.
pub async fn mentions_payload(state: &AppState) -> Value {
    match mentions_payload_with(state, Params::default()).await {
        (StatusCode::OK, v) => v,
        _ => json!({ "candidates": [] }),
    }
}

async fn mentions_payload_with(state: &AppState, p: Params) -> (StatusCode, Value) {
    let cards = {
        let conn = match state.store.read() {
            Ok(c) => c,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    json!({ "error": format!("store unreadable: {e}") }),
                )
            }
        };
        match open_cards(&conn, p.session.as_deref()) {
            Ok(m) => m,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    json!({ "error": format!("board unreadable: {e}") }),
                )
            }
        }
    };

    if cards.is_empty() {
        return (
            StatusCode::OK,
            json!({
                "candidates": [], "open_cards_scanned": 0, "repos": [],
                "verdict": "no open cards to check",
            }),
        );
    }

    // Distinct repos behind the sessions that own these cards. Several sessions
    // routinely share one checkout, so resolve to toplevel and dedupe.
    let dirs: BTreeSet<String> = {
        let conn = match state.store.read() {
            Ok(c) => c,
            Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR,
                json!({ "error": "store unreadable" })),
        };
        match super::sessions_legacy::build_array(&conn) {
            Ok(arr) => arr
                .iter()
                .filter_map(|v| {
                    let name = v["name"].as_str()?;
                    let dir = v["dir"].as_str().unwrap_or("");
                    // Only sessions that own one of these cards.
                    cards
                        .values()
                        .any(|c| c["session"].as_str() == Some(name))
                        .then(|| dir.to_string())
                })
                .filter(|d| !d.is_empty())
                .collect(),
            Err(_) => BTreeSet::new(),
        }
    };

    let mut repos: BTreeSet<String> = BTreeSet::new();
    for d in &dirs {
        if let Some(top) = git_toplevel(d).await {
            repos.insert(top);
        }
    }

    let known: BTreeSet<&str> = cards.keys().map(|s| s.as_str()).collect();
    let mut by_card: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    let mut truncated_any = false;

    for repo in &repos {
        let (hits, truncated) = scan_repo(repo, &known).await;
        truncated_any |= truncated;
        for (sha, subject, ids) in hits {
            for id in ids {
                by_card.entry(id).or_default().push(json!({
                    "sha": sha, "subject": subject, "repo": repo,
                }));
            }
        }
    }

    let candidates: Vec<Value> = by_card
        .into_iter()
        .map(|(id, commits)| {
            let c = &cards[&id];
            json!({
                "id": id,
                "status": c["status"],
                "session": c["session"],
                "title": c["title"],
                "type": c["type"],
                "commits": commits,
            })
        })
        .collect();

    // State the verdict in a sentence. The lesson from /api/logs/analyze is that
    // an endpoint which has already computed the answer should SAY it, rather
    // than leaving every caller to re-derive it from the rows.
    let verdict = if candidates.is_empty() {
        format!(
            "no open card is named in a commit reachable from HEAD ({} scanned across {} repo(s))",
            cards.len(),
            repos.len()
        )
    } else {
        format!(
            "{} of {} open card(s) are named in a merged commit — REVIEW each; a mention is not \
             proof of completion (commits also reference cards for context, partial work and \
             reverts), so confirm before closing",
            candidates.len(),
            cards.len()
        )
    };

    (
        StatusCode::OK,
        json!({
        "candidates": candidates,
        "open_cards_scanned": cards.len(),
        "repos": repos.iter().collect::<Vec<_>>(),
        "truncated": truncated_any,
        "verdict": verdict,
        "note": "read-only: this endpoint never changes a card's status",
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn known<'a>(v: &'a [&'a str]) -> BTreeSet<&'a str> {
        v.iter().copied().collect()
    }

    // The bug this guards is a filter that matches too much: without a boundary
    // check, a short id is a prefix of a longer one and the report names cards
    // no commit mentions. That returns a confident wrong answer, not silence.
    #[test]
    fn a_short_id_does_not_match_inside_a_longer_one() {
        let k = known(&["AMUX-26", "AMUX-2674"]);
        let found = ids_in_text("fix(board): closes AMUX-2674 at last", &k);
        assert!(found.contains("AMUX-2674"));
        assert!(
            !found.contains("AMUX-26"),
            "AMUX-26 must not match inside AMUX-2674; got {found:?}"
        );
    }

    #[test]
    fn ids_are_found_in_the_body_not_only_the_subject() {
        // e679bdb's shape: the subject counts them, the BODY names them.
        let k = known(&["AMUX-2599", "AMUX-2618", "AMUX-9999"]);
        let msg = "fix(hygiene): five carded defects\n\n\
                   WATCHDOG (AMUX-2618) retargeted, not retired.\n\
                   THE 404 TRIO (AMUX-2599): sessions-git reuses build_array.";
        let found = ids_in_text(msg, &k);
        assert!(found.contains("AMUX-2618") && found.contains("AMUX-2599"));
        assert!(!found.contains("AMUX-9999"), "must not invent a mention");
    }

    #[test]
    fn punctuation_and_line_ends_still_bound_a_match() {
        let k = known(&["AC-323"]);
        for msg in ["closes AC-323.", "(AC-323)", "AC-323", "see AC-323, then"] {
            assert!(
                ids_in_text(msg, &k).contains("AC-323"),
                "should match in {msg:?}"
            );
        }
        assert!(
            ids_in_text("AC-3231 is different", &k).is_empty(),
            "a longer id must not register as AC-323"
        );
    }
}
