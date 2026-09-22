//! Universal search and collection completeness (Invariants 32 + 40, RR-0017).
//!
//! One search API spans every entity in the system — tasks, messages,
//! workers, memories, events, logs — and each hit carries provenance, so a
//! result is navigable back to the entity that produced it (Invariant 32).
//!
//! The completeness half is the sharper invariant. From the Python commit
//! history: "we fetched 50 and didn't find it" became "it doesn't exist",
//! because nothing distinguished a complete result from a truncated one.
//! Invariant 40 kills that class: **a response that omits must announce
//! it.** `total >= returned` always, and `truncated` is COMPUTED by the
//! constructor from `total`/`offset`/`returned` — never hand-set, because a
//! hand-set flag is exactly the silent cap it exists to prevent (ethos rule
//! 7: a check that cannot fail is theatre).

use crate::scope::Scope;
use serde::{Deserialize, Serialize};

/// A response claimed completeness it does not have. Rejected at
/// construction: a collection may under-report items only by announcing
/// truncation (Invariant 40).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CompletenessError {
    #[error(
        "total {total} < returned {returned}: a collection cannot contain more items than exist"
    )]
    TotalLessThanReturned { total: u64, returned: u64 },
}

/// One search result. `entity_type`/`entity_id` are strings, not a closed
/// enum: search spans every entity in the system and new entity kinds must
/// be searchable without recompiling core (the same openness argument as
/// `BackendId`, Invariant 8). `provenance` names where the hit came from
/// (which index/tier matched), so a wrong ranking is traceable (ethos rule
/// 4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchHit {
    pub entity_type: String,
    pub entity_id: String,
    /// `None` for unscoped entities (e.g. an org-wide event stream).
    pub scope: Option<Scope>,
    pub title: String,
    pub snippet: String,
    /// Relevance rank; higher is better. FTS5/BM25 in the baseline tier.
    pub rank: f64,
    pub provenance: String,
}

/// A search response. Construct via [`SearchResult::new`], which computes
/// `returned` and `truncated` — see the module docs for why they are never
/// hand-set.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchResult {
    pub hits: Vec<SearchHit>,
    /// Total matching hits, not just this response.
    pub total: u64,
    pub returned: u64,
    pub truncated: bool,
}

impl SearchResult {
    /// Build a result whose completeness claim is derived, not asserted:
    /// `returned = hits.len()`, `truncated = total > returned`. Rejects
    /// `total < hits.len()` (Invariant 40).
    pub fn new(hits: Vec<SearchHit>, total: u64) -> Result<Self, CompletenessError> {
        let returned = hits.len() as u64;
        if total < returned {
            return Err(CompletenessError::TotalLessThanReturned { total, returned });
        }
        Ok(SearchResult {
            hits,
            total,
            returned,
            truncated: total > returned,
        })
    }
}

/// Every list endpoint's envelope (Invariant 40). Construct via
/// [`PagedResponse::new`]; the `truncated` flag derives from
/// `total > offset + returned`, so an API consumer can always distinguish
/// "empty result" from "result truncated before the item you wanted" —
/// the Python system's silent caps are the defect this kills.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PagedResponse<T> {
    pub items: Vec<T>,
    /// Total matching items across all pages.
    pub total: u64,
    pub offset: u64,
    pub limit: u64,
    pub truncated: bool,
}

impl<T> PagedResponse<T> {
    /// Build a page. `truncated` is COMPUTED (`total > offset + returned`),
    /// never accepted from the caller — a constructor parameter would be a
    /// hand-set completeness claim, which is the failure mode itself.
    /// Rejects `total < items.len()`: a page cannot return more items than
    /// the collection claims to hold (Invariant 40: total >= returned).
    pub fn new(
        items: Vec<T>,
        total: u64,
        offset: u64,
        limit: u64,
    ) -> Result<Self, CompletenessError> {
        let returned = items.len() as u64;
        if total < returned {
            return Err(CompletenessError::TotalLessThanReturned { total, returned });
        }
        Ok(PagedResponse {
            items,
            total,
            offset,
            limit,
            // saturating_add: an offset near u64::MAX must not wrap into a
            // false "complete" claim.
            truncated: total > offset.saturating_add(returned),
        })
    }

    /// Items in THIS page. `total - returned()` is what the response omits.
    pub fn returned(&self) -> u64 {
        self.items.len() as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(id: &str, rank: f64) -> SearchHit {
        SearchHit {
            entity_type: "task".into(),
            entity_id: id.into(),
            scope: None,
            title: format!("task {id}"),
            snippet: "…rate limited anthropic…".into(),
            rank,
            provenance: "fts5".into(),
        }
    }

    #[test]
    fn paged_response_rejects_total_less_than_returned() {
        let err = PagedResponse::new(vec!["a", "b", "c"], 2, 0, 10).unwrap_err();
        assert_eq!(
            err,
            CompletenessError::TotalLessThanReturned {
                total: 2,
                returned: 3
            }
        );
    }

    #[test]
    fn truncated_computed_at_boundaries() {
        // Exact fit: offset + returned == total -> complete.
        let p = PagedResponse::new(vec![1, 2, 3], 3, 0, 3).unwrap();
        assert!(!p.truncated);

        // One item beyond the page -> truncated.
        let p = PagedResponse::new(vec![1, 2, 3], 4, 0, 3).unwrap();
        assert!(p.truncated);

        // Last page reached via offset -> complete.
        let p = PagedResponse::new(vec![9, 10], 10, 8, 5).unwrap();
        assert!(!p.truncated);

        // Middle page -> truncated even though the page is full.
        let p = PagedResponse::new(vec![4, 5, 6], 10, 3, 3).unwrap();
        assert!(p.truncated);

        // Empty page past the end: nothing was omitted -> complete.
        let p = PagedResponse::new(Vec::<i32>::new(), 10, 20, 5).unwrap();
        assert!(!p.truncated);

        // Empty collection, empty page -> complete (empty != truncated).
        let p = PagedResponse::new(Vec::<i32>::new(), 0, 0, 5).unwrap();
        assert!(!p.truncated);
    }

    #[test]
    fn truncation_does_not_wrap_at_huge_offsets() {
        let p = PagedResponse::new(vec![1], u64::MAX, u64::MAX, 1).unwrap();
        // offset + returned would overflow; saturating keeps the claim honest.
        assert!(!p.truncated);
    }

    #[test]
    fn search_result_computes_truncated_and_rejects_lies() {
        let r = SearchResult::new(vec![hit("t1", 1.0), hit("t2", 0.5)], 2).unwrap();
        assert_eq!(r.returned, 2);
        assert!(!r.truncated);

        let r = SearchResult::new(vec![hit("t1", 1.0)], 40).unwrap();
        assert!(r.truncated);
        assert_eq!(r.total, 40);

        let err = SearchResult::new(vec![hit("t1", 1.0), hit("t2", 0.5)], 1).unwrap_err();
        assert!(matches!(
            err,
            CompletenessError::TotalLessThanReturned { .. }
        ));
    }

    #[test]
    fn search_result_serde_round_trip() {
        let r = SearchResult::new(
            vec![SearchHit {
                entity_type: "memory".into(),
                entity_id: "mem_01JGXV0000000000000000TEST".into(),
                scope: Some(Scope::Global),
                title: "api-shapes".into(),
                snippet: "…POST /api/board…".into(),
                rank: 0.87,
                provenance: "fts5:memory_entries".into(),
            }],
            3,
        )
        .unwrap();
        let json = serde_json::to_string(&r).unwrap();
        let back: SearchResult = serde_json::from_str(&json).unwrap();
        assert_eq!(r, back);
    }

    #[test]
    fn paged_response_serde_round_trip() {
        let p = PagedResponse::new(vec!["a".to_string(), "b".to_string()], 5, 0, 2).unwrap();
        let json = serde_json::to_string(&p).unwrap();
        let back: PagedResponse<String> = serde_json::from_str(&json).unwrap();
        assert_eq!(p, back);
    }
}
