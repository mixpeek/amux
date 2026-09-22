//! `GET /api/board/themes` — what this board is actually about, and what to
//! call it (AMUX-2640).
//!
//! Ethan: "you should be able to suggest relevant tags per board based on
//! themes. we can cluster tasks by themes as more are added based on semantic
//! similarity."
//!
//! # Why this is not an embedding pipeline
//!
//! The card proposed embedding titles, caching per revision, clustering by
//! similarity, and suggesting THE CLUSTER'S TAG. Measured on the live board
//! before building it:
//!
//! ```text
//! cards with any tag      207 / 4831  (4.3%)
//! distinct tags           35, of which 29 are identifier-shaped (hrsla:…, needs:you)
//! theme-shaped tags       6
//! theme tags on >=3 cards 1  (`hand-raiser-sla`, and it is auto-applied)
//! ```
//!
//! So "suggest the cluster's tag" has exactly ONE tag it could ever propagate.
//! An embedding service, a per-revision cache and a similarity threshold, to
//! spread one auto-applied tag, is a model call doing the work of a `GROUP BY`
//! (ethos rule 2) plus a pinned model that cannot improve (D3).
//!
//! The useful question at 4.3% tagging is the other direction: not "which
//! existing tag fits this cluster" but "what IS this pile of work about, and
//! what should we call it". Titles exist on all 4831 cards, so that question is
//! answerable today, for free, deterministically.
//!
//! # What it does instead
//!
//! Distinctive-term clustering over open card titles. A term earns theme status
//! by appearing on enough cards to be a body of work, but not so many that it
//! describes the whole board — the same mid-frequency band a human eye uses when
//! skimming a column.
//!
//! This is LEXICAL, not semantic, and says so rather than implying otherwise:
//! "cutover" and "migration" stay separate clusters here. That is the honest
//! upgrade path — when the tag vocabulary is large enough that propagation
//! beats discovery, embeddings replace `theme_terms` alone, and every consumer
//! of this endpoint gets better output without changing.

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};

use super::AppState;

/// Words that carry no theme. English function words plus this board's own
/// vocabulary — "fix"/"bug"/"card" describe every second card here, so they
/// would cluster the board into one useless pile.
const STOPWORDS: &[&str] = &[
    "the", "and", "for", "with", "from", "that", "this", "into", "not", "are", "was", "but", "its",
    "has", "have", "had", "will", "can", "cannot", "does", "did", "when", "what", "which", "who",
    "why", "how", "all", "any", "its", "our", "out", "off", "per", "via", "than", "then", "them",
    "they", "there", "their", "been", "being", "were", "only", "also", "just", "now", "new", "old",
    "one", "two", "get", "got", "set", "use", "used", "using", "make", "makes", "made", "should",
    "must", "never", "always", "every", "still", "same", "some", "more", "most", "less",
    // Board vocabulary: true of almost every card, therefore meaningless as a theme.
    "fix", "fixes", "fixed", "bug", "issue", "card", "cards", "board", "amux", "task", "tasks",
    "work", "add", "adds", "added", "update", "updates", "updated", "remove", "removed", "change",
    "changes", "changed", "support", "improve", "wrong", "broken", "fails", "failing", "error",
];

#[derive(Deserialize)]
pub struct Params {
    /// Restrict to one session's cards.
    pub session: Option<String>,
    /// Minimum cards sharing a term before it counts as a theme.
    pub min_size: Option<usize>,
}

/// A term is a theme candidate when it appears on at least `min_size` cards and
/// on no more than this share of them. A term on 60% of the board describes the
/// board, not a theme within it.
const MAX_DOC_SHARE: f64 = 0.25;
const DEFAULT_MIN_SIZE: usize = 3;
const MAX_CLUSTERS: usize = 40;

/// Remove card-id references (`AMUX-2640`, `BACKE-2536`, `TG-2977`) before
/// tokenizing.
///
/// Found only by running this against the REAL board: the top clusters were
/// `mvs` (105 cards), `backe` (38) and `cdna` (18) — every one of them an id
/// PREFIX, not a theme. Splitting `BACKE-2536` on the hyphen leaves `backe`,
/// which then looks like a strong shared term because a whole project's cards
/// carry it. Suggesting `mvs` as a tag is also redundant: it is already in the
/// card's id.
///
/// The synthetic test did not catch this and reported the opposite. It used
/// `AMUX-2640` and asserted no `amux` term came out — which passed only because
/// "amux" is in STOPWORDS. The one prefix I happened to test was the one prefix
/// already handled, so a broken general case looked verified.
fn strip_card_ids(title: &str) -> String {
    let mut out = String::with_capacity(title.len());
    let bytes: Vec<char> = title.chars().collect();
    let mut i = 0;
    while i < bytes.len() {
        // A run of >=2 letters, then '-', then >=1 digit: a card reference.
        let start = i;
        let mut j = i;
        while j < bytes.len() && bytes[j].is_ascii_alphabetic() {
            j += 1;
        }
        let letters = j - i;
        if letters >= 2 && j < bytes.len() && bytes[j] == '-' {
            let mut k = j + 1;
            while k < bytes.len() && bytes[k].is_ascii_digit() {
                k += 1;
            }
            if k > j + 1 {
                out.push(' '); // keep word boundaries intact
                i = k;
                continue;
            }
        }
        out.push(bytes[start]);
        i = start + 1;
    }
    out
}

/// Split a title into theme-candidate terms.
///
/// Kept deliberately dumb and pure so it is testable and cannot depend on the
/// DB: lowercase, split on anything not alphanumeric, drop stopwords, drop
/// short tokens and bare numbers. Identifier-shaped noise (`AMUX-2640`) drops
/// out via the number rule after splitting.
pub fn terms(title: &str) -> BTreeSet<String> {
    strip_card_ids(title)
        .split(|c: char| !c.is_alphanumeric())
        .map(|w| w.trim().to_lowercase())
        .filter(|w| w.len() >= 3)
        .filter(|w| !w.chars().all(|c| c.is_ascii_digit()))
        .filter(|w| !STOPWORDS.contains(&w.as_str()))
        .collect()
}

/// Group cards by their most distinctive shared term.
///
/// Pure over (id, title, tags) so the clustering is testable without a board.
/// Returns clusters sorted by size descending.
pub fn cluster(cards: &[(String, String, Vec<String>)], min_size: usize) -> Vec<Value> {
    if cards.is_empty() {
        return Vec::new();
    }
    let per_card: Vec<(usize, BTreeSet<String>)> = cards
        .iter()
        .enumerate()
        .map(|(i, c)| (i, terms(&c.1)))
        .collect();

    let mut df: HashMap<&str, usize> = HashMap::new();
    for (_, ts) in &per_card {
        for t in ts {
            *df.entry(t.as_str()).or_insert(0) += 1;
        }
    }
    let cap = ((cards.len() as f64) * MAX_DOC_SHARE).ceil() as usize;
    let eligible: BTreeSet<&str> = df
        .iter()
        .filter(|(_, &n)| n >= min_size && n <= cap.max(min_size))
        .map(|(t, _)| *t)
        .collect();

    // Each card joins the cluster of its most COMMON eligible term, so related
    // cards converge instead of splintering on an incidental rare word.
    // Ties break alphabetically, which is what makes this deterministic —
    // a HashMap iteration order would otherwise reshuffle clusters per call.
    let mut groups: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (i, ts) in &per_card {
        let best = ts
            .iter()
            .filter(|t| eligible.contains(t.as_str()))
            .max_by(|a, b| {
                let (na, nb) = (df[a.as_str()], df[b.as_str()]);
                na.cmp(&nb).then_with(|| b.as_str().cmp(a.as_str()))
            });
        if let Some(t) = best {
            groups.entry(t.as_str()).or_default().push(*i);
        }
    }

    let mut out: Vec<Value> = groups
        .into_iter()
        .filter(|(_, m)| m.len() >= min_size)
        .map(|(term, members)| {
            // Tags already on members — this is where an EXISTING tag
            // propagates, when there is one to propagate.
            let mut tag_counts: BTreeMap<&str, usize> = BTreeMap::new();
            for &i in &members {
                for t in &cards[i].2 {
                    *tag_counts.entry(t.as_str()).or_insert(0) += 1;
                }
            }
            let mut existing: Vec<(&str, usize)> = tag_counts.into_iter().collect();
            existing.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
            json!({
                "suggested_tag": term,
                "size": members.len(),
                "existing_tags": existing.iter().take(5)
                    .map(|(t, n)| json!({"tag": t, "cards": n})).collect::<Vec<_>>(),
                "sample": members.iter().take(5)
                    .map(|&i| json!({"id": cards[i].0, "title": cards[i].1}))
                    .collect::<Vec<_>>(),
                "card_ids": members.iter().map(|&i| cards[i].0.clone()).collect::<Vec<_>>(),
            })
        })
        .collect();
    out.sort_by(|a, b| {
        b["size"].as_u64().cmp(&a["size"].as_u64()).then_with(|| {
            a["suggested_tag"]
                .as_str()
                .cmp(&b["suggested_tag"].as_str())
        })
    });
    out.truncate(MAX_CLUSTERS);
    out
}

pub async fn board_themes(
    State(state): State<AppState>,
    Query(p): Query<Params>,
) -> (StatusCode, Json<Value>) {
    let conn = match state.store.read() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("store unreadable: {e}")})),
            )
        }
    };
    // OPEN cards only: a theme is a live body of work. Including 3,800 done
    // cards would surface last month's themes as if they needed tagging.
    let sql = "SELECT id, COALESCE(title,'') FROM issues \
               WHERE deleted IS NULL AND COALESCE(archived,0)=0 \
               AND status IN ('todo','doing','review','backlog') \
               AND (?1 IS NULL OR session = ?1)";
    let rows: Vec<(String, String)> = match conn.prepare(sql).and_then(|mut st| {
        st.query_map(rusqlite::params![p.session.as_deref()], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })
        .map(|rs| rs.flatten().collect())
    }) {
        Ok(v) => v,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("board unreadable: {e}")})),
            )
        }
    };

    let mut tags: HashMap<String, Vec<String>> = HashMap::new();
    if let Ok(mut st) = conn.prepare("SELECT issue_id, tag FROM issue_tags") {
        if let Ok(rs) = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))) {
            for (id, tag) in rs.flatten() {
                tags.entry(id).or_default().push(tag);
            }
        }
    }
    let cards: Vec<(String, String, Vec<String>)> = rows
        .into_iter()
        .map(|(id, title)| {
            let t = tags.get(&id).cloned().unwrap_or_default();
            (id, title, t)
        })
        .collect();

    let min_size = p.min_size.unwrap_or(DEFAULT_MIN_SIZE).max(2);
    let clusters = cluster(&cards, min_size);
    let clustered: usize = clusters
        .iter()
        .map(|c| c["size"].as_u64().unwrap_or(0) as usize)
        .sum();

    (
        StatusCode::OK,
        Json(json!({
            "themes": clusters,
            "open_cards": cards.len(),
            "cards_in_a_theme": clustered,
            "min_size": min_size,
            // Say what this IS, so a caller does not read semantic grouping
            // into a lexical one. "cutover" and "migration" are separate
            // clusters here and that is a known limit, not a bug report.
            "method": "distinctive-term (lexical) over open card titles — not embeddings",
            "note": "suggested_tag is a PROPOSAL derived from titles; existing_tags shows what \
                     members are already tagged, which is what propagates when a real tag exists",
        })),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(id: &str, title: &str, tags: &[&str]) -> (String, String, Vec<String>) {
        (
            id.into(),
            title.into(),
            tags.iter().map(|s| s.to_string()).collect(),
        )
    }

    #[test]
    fn board_vocabulary_is_not_a_theme() {
        // Every card here says "fix" and "bug". Clustering on those would put
        // the whole board in one pile and call it a theme.
        let t = terms("fix bug in the scheduler card");
        assert!(!t.contains("fix") && !t.contains("bug") && !t.contains("card"));
        assert!(t.contains("scheduler"));
    }

    #[test]
    fn card_ids_and_short_tokens_do_not_become_themes() {
        // Deliberately NOT "AMUX-2640": that prefix is in STOPWORDS, so it
        // passes whether or not id-stripping works. This test used it and
        // reported success while `mvs`, `backe` and `cdna` were the three
        // biggest "themes" on the real board. Use prefixes that are NOT
        // stopworded, which is the case the code actually has to handle.
        for id in ["BACKE-2536", "MVS-1204", "TG-2977", "CDNA-88"] {
            let t = terms(&format!("{id} scheduler misfires"));
            let prefix = id.split('-').next().unwrap().to_lowercase();
            assert!(
                !t.contains(&prefix),
                "card-id prefix {prefix:?} leaked from {id:?} as a theme: {t:?}"
            );
            assert!(t.contains("scheduler"), "the real word must survive: {t:?}");
        }
        let t = terms("v2 UI a bc scheduler 2640");
        assert!(!t.contains("2640"), "a bare number is not a theme: {t:?}");
        assert!(!t.contains("a") && !t.contains("bc"));
    }

    /// A hyphenated WORD is not a card id and must survive — the id rule keys
    /// on letters-hyphen-DIGITS, so over-stripping would eat real vocabulary.
    #[test]
    fn hyphenated_words_are_not_mistaken_for_card_ids() {
        let t = terms("read-path vector completeness on multi-tenant shards");
        assert!(t.contains("read") && t.contains("path"), "{t:?}");
        assert!(t.contains("tenant") && t.contains("shards"), "{t:?}");
    }

    #[test]
    fn cards_sharing_a_term_cluster_and_propose_it() {
        let cards = vec![
            card("A", "scheduler misfires on weekends", &[]),
            card("B", "scheduler run history is empty", &[]),
            card("C", "scheduler retries twice", &[]),
            card("D", "browser profile deletion", &[]),
        ];
        let out = cluster(&cards, 3);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0]["suggested_tag"], "scheduler");
        assert_eq!(out[0]["size"], 3);
    }

    /// The one thing an existing tag is good for at 4.3% coverage: showing what
    /// members already carry, so a real tag propagates where one exists.
    #[test]
    fn an_existing_tag_on_members_is_surfaced_for_propagation() {
        let cards = vec![
            card("A", "scheduler misfires", &["ops"]),
            card("B", "scheduler history empty", &["ops"]),
            card("C", "scheduler retries twice", &[]),
        ];
        let out = cluster(&cards, 3);
        assert_eq!(out[0]["existing_tags"][0]["tag"], "ops");
        assert_eq!(out[0]["existing_tags"][0]["cards"], 2);
    }

    /// A term on most of the board describes the BOARD. Without this cap the
    /// largest "theme" is always the least informative word present.
    #[test]
    fn a_term_on_most_cards_is_not_a_theme() {
        let cards: Vec<_> = (0..10)
            .map(|i| card(&format!("C{i}"), "dashboard something", &[]))
            .collect();
        let out = cluster(&cards, 3);
        assert!(
            out.is_empty(),
            "'dashboard' is on 100% of cards and must not be a theme: {out:?}"
        );
    }

    /// Determinism: same input, same clusters, in the same order. Term scoring
    /// runs over a HashMap, so without explicit tie-breaks the output would
    /// reshuffle between calls and the UI would flicker.
    #[test]
    fn clustering_is_stable_across_calls() {
        let cards = vec![
            card("A", "scheduler alpha", &[]),
            card("B", "scheduler beta", &[]),
            card("C", "scheduler gamma", &[]),
            card("D", "browser alpha", &[]),
            card("E", "browser beta", &[]),
            card("F", "browser gamma", &[]),
        ];
        let first = cluster(&cards, 3);
        for _ in 0..20 {
            assert_eq!(
                cluster(&cards, 3),
                first,
                "clustering must be deterministic"
            );
        }
    }

    #[test]
    fn an_empty_board_yields_no_themes_rather_than_panicking() {
        assert!(cluster(&[], 3).is_empty());
    }
}
