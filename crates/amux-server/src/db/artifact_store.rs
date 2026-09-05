//! Artifact registry store (`_amux_task_artifacts`, migration 0046).
//!
//! Separates artifact state from task state: a task points to artifacts (PRs,
//! commits, test runs, docs) and each artifact has its own lifecycle. Gates
//! can evaluate artifact state rather than parsing free-text evidence.

use rusqlite::{params, Connection, OptionalExtension, Row};

#[derive(Debug, Clone)]
pub struct ArtifactRow {
    pub id: String,
    pub task_id: String,
    pub kind: String,
    pub ref_value: String,
    pub state: String,
    pub description: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

pub const ARTIFACT_STATES: &[&str] = &["created", "submitted", "merged", "deployed"];

pub const KNOWN_KINDS: &[&str] = &[
    "implementation",
    "verification",
    "design",
    "doc",
    "screenshot",
    "config",
];

fn row_to_artifact(r: &Row<'_>) -> rusqlite::Result<ArtifactRow> {
    Ok(ArtifactRow {
        id: r.get(0)?,
        task_id: r.get(1)?,
        kind: r.get(2)?,
        ref_value: r.get(3)?,
        state: r.get(4)?,
        description: r.get(5)?,
        created_at: r.get(6)?,
        updated_at: r.get(7)?,
    })
}

pub fn insert(conn: &Connection, row: &ArtifactRow) -> rusqlite::Result<usize> {
    conn.execute(
        "INSERT INTO _amux_task_artifacts \
             (id, task_id, kind, ref_value, state, description, created_at, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            row.id,
            row.task_id,
            row.kind,
            row.ref_value,
            row.state,
            row.description,
            row.created_at,
            row.updated_at,
        ],
    )
}

pub fn get_for_task_ref(
    conn: &Connection,
    task_id: &str,
    ref_value: &str,
) -> rusqlite::Result<Option<ArtifactRow>> {
    conn.query_row(
        "SELECT id, task_id, kind, ref_value, state, description, created_at, updated_at \
         FROM _amux_task_artifacts WHERE task_id = ?1 AND ref_value = ?2 \
         ORDER BY created_at ASC LIMIT 1",
        params![task_id, ref_value],
        row_to_artifact,
    )
    .optional()
}

/// Best-effort kind for references extracted from model-authored output.
///
/// Kind is presentation metadata, not a closed ontology. The reference and
/// exact task id are the durable facts; this only keeps screenshots/docs from
/// being displayed as generic implementation outputs.
pub fn inferred_kind(reference: &str) -> &'static str {
    let path = reference
        .split(['?', '#'])
        .next()
        .unwrap_or(reference)
        .to_ascii_lowercase();
    if [".png", ".jpg", ".jpeg", ".gif", ".webp", ".svg"]
        .iter()
        .any(|ext| path.ends_with(ext))
    {
        "screenshot"
    } else if [".md", ".mdx", ".pdf", ".doc", ".docx", ".txt"]
        .iter()
        .any(|ext| path.ends_with(ext))
    {
        "doc"
    } else {
        "implementation"
    }
}

/// Register references captured from a worker output, once per exact task.
///
/// The store's writer is serialized, so the existence check plus insert is
/// atomic with the card mutation that called it. This deliberately does not
/// infer a task: callers must already have the authoritative card id.
pub fn insert_captured_refs(
    conn: &Connection,
    task_id: &str,
    refs: impl IntoIterator<Item = String>,
    description: &str,
    now: i64,
) -> rusqlite::Result<Vec<ArtifactRow>> {
    let mut inserted = Vec::new();
    for raw in refs {
        let reference = raw.trim();
        if reference.is_empty() || get_for_task_ref(conn, task_id, reference)?.is_some() {
            continue;
        }
        let row = ArtifactRow {
            id: format!("ART-{}", ulid::Ulid::new().to_string().to_lowercase()),
            task_id: task_id.to_string(),
            kind: inferred_kind(reference).to_string(),
            ref_value: reference.to_string(),
            state: "created".to_string(),
            description: Some(description.to_string()),
            created_at: now,
            updated_at: now,
        };
        insert(conn, &row)?;
        inserted.push(row);
    }
    Ok(inserted)
}

pub fn list_for_task(conn: &Connection, task_id: &str) -> rusqlite::Result<Vec<ArtifactRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, task_id, kind, ref_value, state, description, created_at, updated_at \
         FROM _amux_task_artifacts WHERE task_id = ?1 ORDER BY created_at ASC",
    )?;
    let rows = stmt
        .query_map(params![task_id], row_to_artifact)?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub fn get(conn: &Connection, id: &str) -> rusqlite::Result<Option<ArtifactRow>> {
    conn.query_row(
        "SELECT id, task_id, kind, ref_value, state, description, created_at, updated_at \
         FROM _amux_task_artifacts WHERE id = ?1",
        params![id],
        row_to_artifact,
    )
    .optional()
}

pub fn update_state(conn: &Connection, id: &str, state: &str, now: i64) -> rusqlite::Result<usize> {
    if !ARTIFACT_STATES.contains(&state) {
        return Err(rusqlite::Error::InvalidParameterName(format!(
            "invalid artifact state {state:?}, must be one of {ARTIFACT_STATES:?}"
        )));
    }
    conn.execute(
        "UPDATE _amux_task_artifacts SET state = ?2, updated_at = ?3 WHERE id = ?1",
        params![id, state, now],
    )
}

pub fn delete_for_task(conn: &Connection, task_id: &str, id: &str) -> rusqlite::Result<usize> {
    conn.execute(
        "DELETE FROM _amux_task_artifacts WHERE task_id = ?1 AND id = ?2",
        params![task_id, id],
    )
}
