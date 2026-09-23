//! Memory as a durable, scoped, revisioned entity (Invariant 42, RR-0014).
//!
//! The largest architectural gap the Python commit history exposes: canonical
//! memory lived in files, a generated projection (MEMORY.md, a compacted
//! summary) got read back as canonical, and old generated data overwrote
//! newer source — or two workers wrote through one shared file and the last
//! writer won. The fix is structural, not procedural: `MemoryEntry` is the
//! canonical entity, everything else (search index, worker context, compacted
//! summary, MEMORY.md) is a derived, read-only projection (Invariant 39) that
//! never writes back without an explicit user mutation.
//!
//! Two properties carry the invariant:
//! - **Soft delete.** `deleted_at` is a timestamp, never a row removal —
//!   history is never lost, and a delete is as attributable as a write.
//! - **One scope resolver.** Visibility flows through
//!   [`Scope::applies_to`](crate::scope::Scope::applies_to) (Invariant 2),
//!   never a per-subsystem re-implementation — re-implemented inheritance is
//!   how the Python system's env, gate, and memory scoping came to disagree.

use crate::ids::{MemoryId, WorkerId};
use crate::scope::{ResolutionTarget, Scope};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// What kind of memory this is. Mirrors the memory frontmatter types the
/// Python system used in its memory-file headers, so imports map 1:1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryType {
    /// A fact about the user (preferences, corrections addressed to them).
    User,
    /// Feedback the user gave about behavior ("never do X again").
    Feedback,
    /// Knowledge about a project/codebase.
    Project,
    /// Reference material (API shapes, runbooks, how-tos).
    Reference,
}

/// Who wrote this entry. Provenance is part of the answer (ethos rule 4): a
/// wrong memory must be traceable to the party that wrote it, and a
/// worker-written memory carries the durable [`WorkerId`], not a display
/// name that can be renamed out from under the record (Invariant 43).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MemoryProvenance {
    /// The human wrote it directly.
    HumanWritten,
    /// A worker wrote it during a turn.
    WorkerWritten { worker: WorkerId },
    /// Migrated/imported from an external source (e.g. the Python system's
    /// memory files); `source` names where it came from.
    Imported { source: String },
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MemoryError {
    /// Mutating a soft-deleted entry is rejected rather than silently
    /// resurrecting it: `deleted_at` is history, and history is never
    /// overwritten by a later write (Invariant 42).
    #[error("memory {id} is soft-deleted; mutations on deleted entries are rejected")]
    AlreadyDeleted { id: MemoryId },
}

/// A single durable memory. Canonical store is the `memory_entries` table
/// (RR-0019); this type is the row's vocabulary.
///
/// `version` is the entity version (Invariant 35): concurrent writers get a
/// 409 conflict at the API layer, not last-writer-wins. Core only defines
/// the bump discipline; optimistic concurrency lives with the store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryEntry {
    pub id: MemoryId,
    /// Where this memory is visible: global to all, group-shared, or private
    /// to one worker (Invariant 2).
    pub scope: Scope,
    /// Kebab-case slug, unique within its scope.
    pub name: String,
    pub content: String,
    pub memory_type: MemoryType,
    /// Entity version, bumped on every applied mutation (Invariant 35).
    pub version: u64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Soft delete — never lose history. A deleted entry stays in the store
    /// and out of [`visible`].
    pub deleted_at: Option<DateTime<Utc>>,
    pub provenance: MemoryProvenance,
    /// Optional lifecycle boundary for operational/scratch knowledge. Expired
    /// entries stay queryable as history but leave assembled context.
    #[serde(default)]
    pub expires_at: Option<DateTime<Utc>>,
    /// Last time a human or sensor re-confirmed the content.
    #[serde(default)]
    pub last_validated_at: Option<DateTime<Utc>>,
    /// Replacement memory id. Kept as a string so imported legacy ids remain
    /// representable without minting a false native identity.
    #[serde(default)]
    pub superseded_by: Option<String>,
}

impl MemoryEntry {
    /// Create a fresh entry. Core is pure — the timestamp is a parameter,
    /// never a clock read (the caller/server supplies `now`).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: MemoryId,
        scope: Scope,
        name: impl Into<String>,
        content: impl Into<String>,
        memory_type: MemoryType,
        provenance: MemoryProvenance,
        at: DateTime<Utc>,
    ) -> Self {
        MemoryEntry {
            id,
            scope,
            name: name.into(),
            content: content.into(),
            memory_type,
            version: 1,
            created_at: at,
            updated_at: at,
            deleted_at: None,
            provenance,
            expires_at: None,
            last_validated_at: None,
            superseded_by: None,
        }
    }

    pub fn is_deleted(&self) -> bool {
        self.deleted_at.is_some()
    }

    /// Replace the content, bumping `version` and `updated_at`.
    ///
    /// Returns `Ok(true)` if applied, `Ok(false)` for a no-op (identical
    /// content) — the version bumps iff authoritative state changed
    /// (Invariant 37: a no-op that bumps `version` makes "did it change?"
    /// unreliable for every consumer). Errors on a soft-deleted entry
    /// rather than silently resurrecting it.
    pub fn update(
        &mut self,
        content: impl Into<String>,
        at: DateTime<Utc>,
    ) -> Result<bool, MemoryError> {
        if self.is_deleted() {
            return Err(MemoryError::AlreadyDeleted {
                id: self.id.clone(),
            });
        }
        let content = content.into();
        if content == self.content {
            return Ok(false);
        }
        self.content = content;
        self.version += 1;
        self.updated_at = at;
        Ok(true)
    }

    /// Apply content and lifecycle metadata as one versioned mutation.
    pub fn update_lifecycle(
        &mut self,
        content: Option<String>,
        expires_at: Option<DateTime<Utc>>,
        last_validated_at: Option<DateTime<Utc>>,
        superseded_by: Option<String>,
        at: DateTime<Utc>,
    ) -> Result<bool, MemoryError> {
        if self.is_deleted() {
            return Err(MemoryError::AlreadyDeleted {
                id: self.id.clone(),
            });
        }
        let mut changed = false;
        if let Some(content) = content {
            if content != self.content {
                self.content = content;
                changed = true;
            }
        }
        if expires_at.is_some() && expires_at != self.expires_at {
            self.expires_at = expires_at;
            changed = true;
        }
        if last_validated_at.is_some() && last_validated_at != self.last_validated_at {
            self.last_validated_at = last_validated_at;
            changed = true;
        }
        if superseded_by.is_some() && superseded_by != self.superseded_by {
            self.superseded_by = superseded_by;
            changed = true;
        }
        if changed {
            self.version += 1;
            self.updated_at = at;
        }
        Ok(changed)
    }

    /// Soft-delete: set `deleted_at`, keep the row. Double-delete is an
    /// error, not a no-op — a second delete arriving means two actors
    /// believed they owned the entry's lifecycle, which is worth surfacing,
    /// not swallowing (ethos rule 4: a wrong answer must be detectable).
    pub fn soft_delete(&mut self, at: DateTime<Utc>) -> Result<(), MemoryError> {
        if self.is_deleted() {
            return Err(MemoryError::AlreadyDeleted {
                id: self.id.clone(),
            });
        }
        self.deleted_at = Some(at);
        self.version += 1;
        self.updated_at = at;
        Ok(())
    }
}

/// The memories `target` may see: scope-filtered through the ONE resolver
/// (Invariant 2) and excluding soft-deleted entries. A worker's memory is
/// private to that worker; group memory is shared within the group; global
/// memory is visible to all (Invariant 42). This is the only visibility
/// predicate — context assembly, the API, and projections all call it, so
/// they cannot drift apart.
pub fn visible<'a>(entries: &'a [MemoryEntry], target: &ResolutionTarget) -> Vec<&'a MemoryEntry> {
    entries
        .iter()
        .filter(|e| !e.is_deleted() && e.scope.applies_to(target))
        .collect()
}

/// Visibility at a particular instant, including expiry and supersession.
/// The instant is supplied so core remains deterministic.
pub fn visible_at<'a>(
    entries: &'a [MemoryEntry],
    target: &ResolutionTarget,
    now: DateTime<Utc>,
) -> Vec<&'a MemoryEntry> {
    entries
        .iter()
        .filter(|e| {
            !e.is_deleted()
                && e.superseded_by.is_none()
                && e.expires_at.is_none_or(|expiry| expiry > now)
                && e.scope.applies_to(target)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem_id(ulid: &str) -> MemoryId {
        MemoryId::from_ulid(ulid.parse().unwrap())
    }

    fn worker(ulid: &str) -> WorkerId {
        WorkerId::from_ulid(ulid.parse().unwrap())
    }

    fn t0() -> DateTime<Utc> {
        "2026-08-01T00:00:00Z".parse().unwrap()
    }

    fn t1() -> DateTime<Utc> {
        "2026-08-01T01:00:00Z".parse().unwrap()
    }

    fn entry_for(scope: Scope, ulid: &str) -> MemoryEntry {
        MemoryEntry::new(
            mem_id(ulid),
            scope,
            "test-memory",
            "the content",
            MemoryType::Project,
            MemoryProvenance::HumanWritten,
            t0(),
        )
    }

    #[test]
    fn crud_shape_and_serde_round_trip() {
        let w = worker("01JGXV0000000000000000TEST");
        let e = MemoryEntry::new(
            mem_id("01JGXV0000000000000000TEST"),
            Scope::Worker { id: w.clone() },
            "api-shapes",
            "POST /api/board takes title+status",
            MemoryType::Reference,
            MemoryProvenance::WorkerWritten { worker: w },
            t0(),
        );
        assert_eq!(e.version, 1);
        assert_eq!(e.created_at, e.updated_at);
        assert!(e.deleted_at.is_none());
        assert!(e.expires_at.is_none());

        let json = serde_json::to_string(&e).unwrap();
        let back: MemoryEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(e, back);
    }

    #[test]
    fn update_bumps_version_and_updated_at() {
        let mut e = entry_for(Scope::Global, "01JGXV0000000000000000TEST");
        let applied = e.update("new content", t1()).unwrap();
        assert!(applied);
        assert_eq!(e.version, 2);
        assert_eq!(e.updated_at, t1());
        assert_eq!(e.created_at, t0());
        assert_eq!(e.content, "new content");
    }

    #[test]
    fn noop_update_does_not_bump_version() {
        // Invariant 37: version increments iff authoritative state changed.
        let mut e = entry_for(Scope::Global, "01JGXV0000000000000000TEST");
        let applied = e.update("the content", t1()).unwrap();
        assert!(!applied);
        assert_eq!(e.version, 1);
        assert_eq!(e.updated_at, t0());
    }

    #[test]
    fn scope_isolation_worker_a_cannot_see_worker_b() {
        let a = worker("01JGXV0000000000000000AAAA");
        let b = worker("01JGXV0000000000000000BBBB");
        let entries = vec![
            entry_for(
                Scope::Worker { id: a.clone() },
                "01JGXV0000000000000000AAAA",
            ),
            entry_for(
                Scope::Worker { id: b.clone() },
                "01JGXV0000000000000000BBBB",
            ),
            entry_for(Scope::Global, "01JGXV0000000000000000TEST"),
        ];

        let target_a = ResolutionTarget {
            worker: Some(a.clone()),
            group: None,
        };
        let seen: Vec<_> = visible(&entries, &target_a);
        // A sees its own memory and the global one — never B's.
        assert_eq!(seen.len(), 2);
        assert!(seen
            .iter()
            .all(|e| e.scope != Scope::Worker { id: b.clone() }));
        assert!(seen
            .iter()
            .any(|e| e.scope == Scope::Worker { id: a.clone() }));
        assert!(seen.iter().any(|e| e.scope == Scope::Global));
    }

    #[test]
    fn expired_and_superseded_memories_leave_context_but_remain_history() {
        let mut expired = entry_for(Scope::Global, "01JGXV0000000000000000AAAA");
        expired.expires_at = Some(t0());
        let mut superseded = entry_for(Scope::Global, "01JGXV0000000000000000BBBB");
        superseded.superseded_by = Some("mem_replacement".into());
        let live = entry_for(Scope::Global, "01JGXV0000000000000000CCCC");
        let all = vec![expired, superseded, live];
        let visible = visible_at(&all, &ResolutionTarget::default(), t1());
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].id, all[2].id);
    }

    #[test]
    fn soft_delete_excluded_from_visible() {
        let mut e = entry_for(Scope::Global, "01JGXV0000000000000000TEST");
        e.soft_delete(t1()).unwrap();
        assert_eq!(e.version, 2);
        assert_eq!(e.deleted_at, Some(t1()));

        let entries = vec![e];
        let target = ResolutionTarget::default();
        assert!(visible(&entries, &target).is_empty());
    }

    #[test]
    fn double_delete_errors() {
        let mut e = entry_for(Scope::Global, "01JGXV0000000000000000TEST");
        e.soft_delete(t1()).unwrap();
        let err = e.soft_delete(t1()).unwrap_err();
        assert!(matches!(err, MemoryError::AlreadyDeleted { .. }));
        // The failed delete must not have touched the entry.
        assert_eq!(e.version, 2);
    }

    #[test]
    fn update_of_deleted_entry_errors() {
        let mut e = entry_for(Scope::Global, "01JGXV0000000000000000TEST");
        e.soft_delete(t1()).unwrap();
        let err = e.update("resurrected?", t1()).unwrap_err();
        assert!(matches!(err, MemoryError::AlreadyDeleted { .. }));
        assert_eq!(e.content, "the content");
    }
}
