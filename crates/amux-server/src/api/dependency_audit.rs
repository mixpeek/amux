//! `GET /api/debug/dependency-audit` (AMUX-4682) — the deterministic answer
//! to "is a cited blocker still real?"
//!
//! Ethan, 2026-09-17: after I explained that a worker's `blocked_on`/
//! `desc` prose can cite a dependency that has already resolved (tubescience
//! citing MG-1829/MC-2063 as live blockers a full day after both went
//! `done`) or cite something that never existed as a board card at all
//! ("SEC-110" — real, but tracked as a security-findings markdown file, not
//! a card, so no board query can confirm or deny it), he asked for the
//! AMUX-HARNESS DETERMINISTIC way of catching this — not another agent
//! re-reading cards on a schedule and hoping it notices. This is that: pure
//! computation over the board, no model call anywhere in it.
//!
//! Three checks, in increasing uncertainty, each true to what it can and
//! cannot know (ethos rule 4 — publish whether the measurement ran, and
//! never collapse "resolved" and "unverifiable" into one bucket):
//!
//! 1. **`stale_structured_deps`** — a card's `depends_on` names a real card
//!    id whose status is now terminal (done/verified/discarded). Zero
//!    false-positive risk: `depends_on` is a structured id array, not prose.
//!    HIGH confidence.
//! 2. **`stale_text_citations`** — `blocked_on`/`source_ref`/the tail of
//!    `desc` mentions an ID-shaped token (`[A-Z]{2,6}-\d{1,6}`) that DOES
//!    resolve to a real card, and that card is terminal. MEDIUM confidence:
//!    prose can legitimately describe history ("waits until X, which is now
//!    done" is sometimes deliberately left as a record), so this is a
//!    candidate list for a human/worker to re-check, not an auto-fix.
//! 3. **`off_board_citations`** — the same token shape, but it matches NO
//!    id anywhere on the board (terminal, non-terminal, archived, all of
//!    it). This is NOT "phantom" or "made up" — SEC-110 lands here and is
//!    completely real, just not board-tracked. It means exactly one thing:
//!    the board cannot confirm or deny this citation, so whoever reads it
//!    needs to check the other system it actually lives in. LOW confidence
//!    by construction, and labeled as such rather than asserted either way.
//!
//! Deliberately NOT auto-mutating anything. `depends_on` is sometimes only
//! one strand of a richer gate (a card's own `gate` criteria can name a
//! DIFFERENT still-open prerequisite than what `depends_on` lists —
//! confirmed live on TUBES-2461, whose `depends_on` pointed at an already-
//! `verified` card while its `gate` criteria correctly still named a
//! different, still-`backlog` one). Auto-clearing a stale `depends_on`
//! could make a card look ready when a real gate elsewhere still holds it.
//! This endpoint reports; a human or the owning worker decides.

use super::AppState;
use crate::db::board_store::{self as bs, ArchivedFilter, IssueRow};
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use serde_json::json;
use std::collections::{HashMap, HashSet};

pub fn routes() -> Router<AppState> {
    Router::new().route("/api/debug/dependency-audit", axum::routing::get(audit))
}

/// Board-vocabulary id shape: 2-6 uppercase letters, a hyphen, 1-6 digits.
/// Matches every prefix actually seen on this board (MI, TUBES, MF, MG, MC,
/// MOS, SP, BACKE, TG, ETHAN, ...) without hardcoding the list, so a NEW
/// worker's prefix is covered the day it appears. Deliberately loose enough
/// to also catch a non-board token like "SEC-110" (that is the point of the
/// off_board_citations bucket) and loose enough to catch some real noise
/// too (a version string, "P0-P2") — the response's `note` field says so;
/// `off_board_citations` is a candidate list, never an assertion.
fn extract_id_tokens(text: &str) -> HashSet<String> {
    let bytes = text.as_bytes();
    let mut out = HashSet::new();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i].is_ascii_uppercase() {
            let start = i;
            let mut j = i + 1;
            while j < bytes.len() && bytes[j].is_ascii_uppercase() {
                j += 1;
            }
            let letters = j - start;
            if (2..=6).contains(&letters) && j < bytes.len() && bytes[j] == b'-' {
                let dash = j;
                let mut k = dash + 1;
                while k < bytes.len() && bytes[k].is_ascii_digit() {
                    k += 1;
                }
                let digits = k - (dash + 1);
                if (1..=6).contains(&digits) {
                    // A real word boundary on both ends -- "XSEC-110" or
                    // "SEC-1101a" must not match a truncated token.
                    let before_ok = start == 0 || !bytes[start - 1].is_ascii_alphanumeric();
                    let after_ok = k == bytes.len() || !bytes[k].is_ascii_alphanumeric();
                    if before_ok && after_ok {
                        out.insert(text[start..k].to_string());
                    }
                }
            }
            i = j.max(i + 1);
        } else {
            i += 1;
        }
    }
    out
}

/// The tail of `desc` a citation is actually likely to be a CURRENT claim
/// in, not the full multi-thousand-char history every one of these cards
/// accumulates (AMUX-4661/4682 read dozens of them this session: the
/// pattern is always append-only, newest last).
const DESC_TAIL_CHARS: usize = 2000;

fn candidate_text(row: &IssueRow) -> String {
    let mut s = String::new();
    if let Some(b) = &row.blocked_on {
        s.push_str(b);
        s.push('\n');
    }
    if let Some(r) = &row.source_ref {
        s.push_str(r);
        s.push('\n');
    }
    let desc_tail: String = row
        .desc
        .chars()
        .rev()
        .take(DESC_TAIL_CHARS)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    s.push_str(&desc_tail);
    s
}

async fn audit(State(state): State<AppState>) -> Response {
    let conn = match state.store.read() {
        Ok(c) => c,
        Err(e) => {
            return (
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                Json(crate::api::measured::unmeasured(
                    json!({"error": e.to_string()}),
                    "the store could not be opened, so no card was read",
                )),
            )
                .into_response()
        }
    };
    // The FULL id -> status map (every status, archived included): a
    // citation is only "off-board" if it matches NOTHING here, and a
    // dependency is only "stale" (not "broken") if the id it names is a
    // real card that has simply finished.
    let all_rows = match bs::list_issues(&conn, &[], &[], ArchivedFilter::All) {
        Ok(r) => r,
        Err(e) => {
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                Json(crate::api::measured::unmeasured(
                    json!({"error": e.to_string()}),
                    "the board query failed, so no card was read",
                )),
            )
                .into_response()
        }
    };
    let status_by_id: HashMap<&str, &str> = all_rows
        .iter()
        .map(|r| (r.id.as_str(), r.status.as_str()))
        .collect();

    let candidates: Vec<&IssueRow> = all_rows
        .iter()
        .filter(|r| r.archived == 0 && !bs::is_terminal_status(&r.status))
        .collect();

    let mut stale_structured_deps = Vec::new();
    let mut stale_text_citations = Vec::new();
    let mut off_board_citations = Vec::new();

    for row in &candidates {
        for dep_id in &row.depends_on {
            if let Some(target_status) = status_by_id.get(dep_id.as_str()) {
                if bs::is_terminal_status(target_status) {
                    stale_structured_deps.push(json!({
                        "card": row.id, "session": row.session, "status": row.status,
                        "depends_on": dep_id, "target_status": target_status,
                    }));
                }
            }
            // A depends_on id that resolves to NOTHING is a data-integrity
            // question (a real id was renamed/merged/deleted), a different
            // and rarer class than this endpoint is built for -- named here
            // rather than silently folded into off_board_citations, which
            // is specifically about PROSE tokens, not structured fields.
        }

        let text = candidate_text(row);
        for token in extract_id_tokens(&text) {
            if token == row.id {
                continue;
            }
            match status_by_id.get(token.as_str()) {
                Some(target_status) if bs::is_terminal_status(target_status) => {
                    stale_text_citations.push(json!({
                        "card": row.id, "session": row.session, "status": row.status,
                        "cited_id": token, "target_status": target_status,
                    }));
                }
                Some(_) => {} // cites a real, still-open card -- nothing to report
                None => {
                    off_board_citations.push(json!({
                        "card": row.id, "session": row.session, "status": row.status,
                        "cited_token": token,
                    }));
                }
            }
        }
    }

    Json(crate::api::measured::measured(
        json!({
            "stale_structured_deps": stale_structured_deps,
            "stale_text_citations": stale_text_citations,
            "off_board_citations": off_board_citations,
            "note": "stale_* = HIGH/MEDIUM confidence, a board id this endpoint confirmed is \
                     terminal. off_board_citations = LOW confidence and NOT a claim the \
                     citation is fake -- it means only that no board card matches the token, \
                     so whatever system it really lives in (a doc, an external tracker) has \
                     to be checked directly. Nothing here is auto-applied: depends_on can be \
                     one strand of a richer gate, so clearing it is a human/worker call, not \
                     this endpoint's.",
        }),
        candidates.len(),
    ))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_relative_reference_has_no_false_starts() {
        assert_eq!(extract_id_tokens("nothing to see here"), HashSet::new());
    }

    #[test]
    fn a_real_shaped_token_is_extracted_with_its_boundaries_respected() {
        let got = extract_id_tokens("frozen behind the SEC-110 credential rotation");
        assert!(got.contains("SEC-110"), "{got:?}");
        // A token glued to other letters on either side is not a citation.
        let glued = extract_id_tokens("XSEC-110a and prefixSEC-110");
        assert!(glued.is_empty(), "{glued:?}");
    }

    #[test]
    fn several_tokens_in_one_blob_are_all_found() {
        let got = extract_id_tokens("depends on MG-1829, MC-2063 and also ETHAN-77");
        assert_eq!(
            got,
            HashSet::from([
                "MG-1829".to_string(),
                "MC-2063".to_string(),
                "ETHAN-77".to_string()
            ])
        );
    }

    #[test]
    fn a_prefix_outside_two_to_six_letters_does_not_match() {
        // Single-letter and seven-letter prefixes are outside the board's
        // real vocabulary (shortest live prefix is 2, e.g. MC/MF/MG/MI/MS/SP).
        let got = extract_id_tokens("A-1 and TOOLONGX-1 do not count, TUBES-1 does");
        assert_eq!(got, HashSet::from(["TUBES-1".to_string()]));
    }

    #[test]
    fn digits_outside_one_to_six_do_not_match() {
        let got = extract_id_tokens("MI- has no digits, MI-1234567 has seven");
        assert!(got.is_empty(), "{got:?}");
    }
}
