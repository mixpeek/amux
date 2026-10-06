//! Episode records (AMUX-5626): one self-contained row per task attempt.
//!
//! # Why
//!
//! A task attempt is the natural unit for asking "did this go well, and what
//! did the worker start from": for judging a harness change, for comparing
//! models on a kind of card, and for anything that later wants to replay or
//! learn from the work. Until now the pieces lived apart and decayed
//! separately. Measured 2026-10-06: 6,422 archived transcripts, none carrying
//! a card or attempt id; 2,854 attempts with an outcome and nothing saying
//! which conversation or which commit they ran against; `card_contracts.sha`
//! one per CARD, overwritten by each attempt; join rows pruned at 14 to 90
//! days. Every day without the join is a day of episodes that cannot be
//! rebuilt afterwards.
//!
//! # What a row holds
//!
//! START (captured within one tick of the claim):
//! the card as the worker was given it (title, description, type, acceptance
//! criteria), the worker's directory, git sha and branch, how many files were
//! already dirty, the provider conversation id (the link to the transcript
//! archive, which is keyed by it), provider and model, a hash of each env
//! layer and of the worker's memory file, the amux build and the harness
//! version.
//!
//! END (filled within one tick of the attempt closing):
//! the attempt's outcome, status, actor and reason, the end sha, commits and
//! diff stat since the start sha, the card's final status and evidence, and
//! the archived transcripts that overlap the attempt's time window.
//!
//! Env layers and memory are stored as HASHES, never contents: layers hold
//! credentials, and the question an episode asks of them is "was it the same
//! configuration", which a hash answers.
//!
//! # Honest about lateness
//!
//! The lease change that opens an attempt happens inside a database write, so
//! git and the filesystem cannot be read there. This job reads them up to one
//! tick later, and the row says how late: `start_lag_s` is seconds between
//! the claim and the capture, and `start_after_end` is 1 when the attempt had
//! already closed by then (a start sha read at that point is the END state and
//! must not be trusted as the start). An attempt older than
//! [`START_CAPTURE_MAX_AGE_S`] with no row is never backfilled: its start
//! state is unknowable, and a guessed one would be a false label.
//!
//! # Not a new primitive
//!
//! A materialized join over stores that already exist (`task_attempts`,
//! `issues`, `trace_archive`, session meta). No reader depends on it yet.
//!
//! Log signals: `verdict="episode_started"` / `"episode_closed"` per row, and
//! `"episodes_tick"` with counts whenever a tick wrote anything.

use crate::api::session_verbs as sv;
use crate::api::AppState;
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

const JOB: &str = super::registry::ids::EPISODES;
const TICK_SECS: u64 = 15;
/// Rows handled per tick, so a backlog cannot hold the writer.
const BATCH: usize = 40;
/// An attempt with no episode row older than this is left alone (see module
/// docs: its start state can no longer be observed).
const START_CAPTURE_MAX_AGE_S: i64 = 3600;
/// Card text kept per episode. A handful of cards carry 300k+ descriptions;
/// the episode keeps the head and records the true length beside it.
const CARD_TEXT_MAX: usize = 20_000;

pub fn tick_secs() -> u64 {
    std::env::var(super::per_job_disable_var(JOB))
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|s: &u64| *s >= 5)
        .unwrap_or(TICK_SECS)
}

/// Created at the point of use for the same reason `task_attempts` is (see
/// `db/attempts.rs`): a numbered migration can be skipped on a box whose
/// version counter was advanced by a dirty build.
pub fn ensure_table(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS task_episodes (
            attempt_id       INTEGER PRIMARY KEY,
            card             TEXT    NOT NULL,
            attempt          INTEGER NOT NULL,
            worker           TEXT    NOT NULL,
            started_at       INTEGER NOT NULL,
            start_captured_at INTEGER NOT NULL,
            start_lag_s      INTEGER NOT NULL,
            start_after_end  INTEGER NOT NULL DEFAULT 0,
            card_start       TEXT    NOT NULL,
            work_dir         TEXT    NOT NULL DEFAULT '',
            start_sha        TEXT    NOT NULL DEFAULT '',
            start_branch     TEXT    NOT NULL DEFAULT '',
            start_dirty      INTEGER,
            conv_id          TEXT    NOT NULL DEFAULT '',
            provider         TEXT    NOT NULL DEFAULT '',
            model            TEXT    NOT NULL DEFAULT '',
            env_layers       TEXT    NOT NULL DEFAULT '[]',
            memory_sha256    TEXT    NOT NULL DEFAULT '',
            amux_build       TEXT    NOT NULL DEFAULT '',
            harness_version  TEXT    NOT NULL DEFAULT '',
            ended_at         INTEGER,
            end_captured_at  INTEGER,
            outcome          TEXT,
            to_status        TEXT,
            ended_by         TEXT,
            reason           TEXT,
            end_sha          TEXT,
            commits          INTEGER,
            diff_stat        TEXT,
            card_end         TEXT,
            transcripts      TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_task_episodes_card ON task_episodes(card, attempt);
        CREATE INDEX IF NOT EXISTS idx_task_episodes_worker ON task_episodes(worker, started_at);
        CREATE INDEX IF NOT EXISTS idx_task_episodes_open ON task_episodes(attempt_id) WHERE ended_at IS NULL;",
    )
}

/// An attempt that needs its start state captured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PendingStart {
    pub attempt_id: i64,
    pub card: String,
    pub attempt: i64,
    pub worker: String,
    pub started_at: i64,
    pub ended_at: Option<i64>,
}

/// What can only be read outside the database.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct WorldState {
    pub work_dir: String,
    pub sha: String,
    pub branch: String,
    pub dirty: Option<i64>,
    pub conv_id: String,
    pub provider: String,
    pub model: String,
    pub env_layers: Value,
    pub memory_sha256: String,
}

fn missing_table(e: &rusqlite::Error, table: &str) -> bool {
    e.to_string().contains(&format!("no such table: {table}"))
}

/// Attempts young enough to capture that have no episode row yet.
pub(crate) fn pending_starts(conn: &Connection, now: i64) -> rusqlite::Result<Vec<PendingStart>> {
    let run = || -> rusqlite::Result<Vec<PendingStart>> {
        let mut st = conn.prepare(
            "SELECT a.id, a.card, a.attempt, a.worker, a.started_at, a.ended_at \
             FROM task_attempts a LEFT JOIN task_episodes e ON e.attempt_id = a.id \
             WHERE e.attempt_id IS NULL AND a.started_at >= ?1 \
             ORDER BY a.id ASC LIMIT ?2",
        )?;
        let rows = st.query_map(params![now - START_CAPTURE_MAX_AGE_S, BATCH as i64], |r| {
            Ok(PendingStart {
                attempt_id: r.get(0)?,
                card: r.get(1)?,
                attempt: r.get(2)?,
                worker: r.get(3)?,
                started_at: r.get(4)?,
                ended_at: r.get(5)?,
            })
        })?;
        rows.collect()
    };
    match run() {
        // No lease has changed since attempts shipped: nothing to record.
        Err(e) if missing_table(&e, "task_attempts") => Ok(Vec::new()),
        other => other,
    }
}

fn clip_text(s: &str) -> (String, usize) {
    let n = s.chars().count();
    if n <= CARD_TEXT_MAX {
        (s.to_string(), n)
    } else {
        (s.chars().take(CARD_TEXT_MAX).collect(), n)
    }
}

/// The card as it stands now, as JSON. `fields` picks start or end shape.
fn card_snapshot(conn: &Connection, card: &str, end: bool) -> Value {
    let row = conn.query_row(
        "SELECT title, COALESCE(desc,''), COALESCE(type,''), COALESCE(status,''), \
                acceptance_criteria, evidence, COALESCE(epic,''), COALESCE(depends_on,'') \
         FROM issues WHERE id = ?1",
        [card],
        |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, Option<String>>(5)?,
                r.get::<_, String>(6)?,
                r.get::<_, String>(7)?,
            ))
        },
    );
    let Ok((title, desc, typ, status, criteria, evidence, epic, deps)) = row else {
        return json!({"missing": true});
    };
    let (desc, desc_chars) = clip_text(&desc);
    if end {
        let (evidence, evidence_chars) = clip_text(evidence.as_deref().unwrap_or(""));
        json!({"status": status, "evidence": evidence, "evidence_chars": evidence_chars,
               "desc": desc, "desc_chars": desc_chars})
    } else {
        json!({"title": title, "desc": desc, "desc_chars": desc_chars, "type": typ,
               "status": status, "acceptance_criteria": criteria, "epic": epic,
               "depends_on": deps})
    }
}

/// Insert the start half. Idempotent: a second capture of the same attempt is
/// ignored, so a tick that overlaps a restart cannot overwrite the first read.
pub(crate) fn insert_start(
    conn: &Connection,
    p: &PendingStart,
    w: &WorldState,
    amux_build: &str,
    now: i64,
) -> rusqlite::Result<bool> {
    ensure_table(conn)?;
    let harness = crate::db::harness_store::harness_version(conn).unwrap_or_default();
    let card_start = card_snapshot(conn, &p.card, false);
    let after_end = p.ended_at.is_some_and(|e| e <= now);
    let n = conn.execute(
        "INSERT OR IGNORE INTO task_episodes \
           (attempt_id, card, attempt, worker, started_at, start_captured_at, start_lag_s, \
            start_after_end, card_start, work_dir, start_sha, start_branch, start_dirty, conv_id, \
            provider, model, env_layers, memory_sha256, amux_build, harness_version) \
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20)",
        params![
            p.attempt_id,
            p.card,
            p.attempt,
            p.worker,
            p.started_at,
            now,
            (now - p.started_at).max(0),
            after_end as i64,
            card_start.to_string(),
            w.work_dir,
            w.sha,
            w.branch,
            w.dirty,
            w.conv_id,
            w.provider,
            w.model,
            w.env_layers.to_string(),
            w.memory_sha256,
            amux_build,
            harness,
        ],
    )?;
    Ok(n == 1)
}

/// An episode whose attempt has ended and whose end half is not written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PendingEnd {
    pub attempt_id: i64,
    pub card: String,
    pub worker: String,
    pub started_at: i64,
    pub ended_at: i64,
    pub outcome: Option<String>,
    pub to_status: Option<String>,
    pub ended_by: Option<String>,
    pub reason: Option<String>,
    pub work_dir: String,
    pub start_sha: String,
    pub conv_id: String,
}

pub(crate) fn pending_ends(conn: &Connection) -> rusqlite::Result<Vec<PendingEnd>> {
    let run = || -> rusqlite::Result<Vec<PendingEnd>> {
        let mut st = conn.prepare(
            "SELECT e.attempt_id, e.card, e.worker, e.started_at, a.ended_at, a.outcome, \
                    a.to_status, a.ended_by, a.reason, e.work_dir, e.start_sha, e.conv_id \
             FROM task_episodes e JOIN task_attempts a ON a.id = e.attempt_id \
             WHERE e.ended_at IS NULL AND a.ended_at IS NOT NULL \
             ORDER BY e.attempt_id ASC LIMIT ?1",
        )?;
        let rows = st.query_map([BATCH as i64], |r| {
            Ok(PendingEnd {
                attempt_id: r.get(0)?,
                card: r.get(1)?,
                worker: r.get(2)?,
                started_at: r.get(3)?,
                ended_at: r.get(4)?,
                outcome: r.get(5)?,
                to_status: r.get(6)?,
                ended_by: r.get(7)?,
                reason: r.get(8)?,
                work_dir: r.get(9)?,
                start_sha: r.get(10)?,
                conv_id: r.get(11)?,
            })
        })?;
        rows.collect()
    };
    match run() {
        Err(e) if missing_table(&e, "task_episodes") || missing_table(&e, "task_attempts") => Ok(Vec::new()),
        other => other,
    }
}

/// Archived transcripts that belong to this attempt: the worker's, either the
/// conversation it started in or any whose span overlaps the attempt window (a
/// conversation reset mid-attempt starts a new one). `first_ts`/`last_ts` are
/// ISO-8601 text, so the window is compared as text in the same format.
fn transcripts_for(conn: &Connection, p: &PendingEnd) -> Value {
    let iso = |t: i64| {
        chrono::DateTime::from_timestamp(t, 0)
            .map(|d| d.format("%Y-%m-%dT%H:%M:%S").to_string())
            .unwrap_or_default()
    };
    let (from, to) = (iso(p.started_at), iso(p.ended_at));
    let run = || -> rusqlite::Result<Vec<Value>> {
        let mut st = conn.prepare(
            "SELECT conv_id, provider, kind, archive_path, source_path, first_ts, last_ts, records \
             FROM trace_archive \
             WHERE (conv_id != '' AND conv_id = ?2) \
                OR (worker = ?1 AND first_ts != '' AND last_ts != '' AND first_ts <= ?4 AND last_ts >= ?3) \
             ORDER BY first_ts ASC LIMIT 50",
        )?;
        let rows = st.query_map(params![p.worker, p.conv_id, from, to], |r| {
            Ok(json!({
                "conv_id": r.get::<_, String>(0)?, "provider": r.get::<_, String>(1)?,
                "kind": r.get::<_, String>(2)?, "archive_path": r.get::<_, String>(3)?,
                "source_path": r.get::<_, String>(4)?, "first_ts": r.get::<_, String>(5)?,
                "last_ts": r.get::<_, String>(6)?, "records": r.get::<_, i64>(7)?,
            }))
        })?;
        rows.collect()
    };
    match run() {
        Ok(rows) => json!({"measured": true, "n_considered": rows.len(), "archived": rows}),
        // The archive lags the live transcript by design; an unreadable or
        // absent archive is recorded as unmeasured, never as "no transcript".
        Err(e) => json!({"measured": false, "n_considered": 0, "why_unmeasured": e.to_string()}),
    }
}

/// What git says about the work between the start sha and now.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct EndWorld {
    pub sha: String,
    pub commits: Option<i64>,
    pub diff_stat: String,
}

pub(crate) fn write_end(conn: &Connection, p: &PendingEnd, w: &EndWorld, now: i64) -> rusqlite::Result<bool> {
    let card_end = card_snapshot(conn, &p.card, true);
    let transcripts = transcripts_for(conn, p);
    let n = conn.execute(
        "UPDATE task_episodes SET ended_at=?2, end_captured_at=?3, outcome=?4, to_status=?5, \
                ended_by=?6, reason=?7, end_sha=?8, commits=?9, diff_stat=?10, card_end=?11, \
                transcripts=?12 \
         WHERE attempt_id=?1 AND ended_at IS NULL",
        params![
            p.attempt_id,
            p.ended_at,
            now,
            p.outcome,
            p.to_status,
            p.ended_by,
            p.reason,
            w.sha,
            w.commits,
            w.diff_stat,
            card_end.to_string(),
            transcripts.to_string(),
        ],
    )?;
    Ok(n == 1)
}

// ---------------------------------------------------------------------------
// The world outside the database
// ---------------------------------------------------------------------------

fn git(dir: &str, args: &[&str]) -> Option<String> {
    if dir.is_empty() || !std::path::Path::new(dir).is_dir() {
        return None;
    }
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn sha256_file(path: &std::path::Path) -> String {
    std::fs::read(path)
        .map(|b| hex::encode(Sha256::digest(&b)))
        .unwrap_or_default()
}

pub(crate) fn world_at(dir: &str) -> (String, String, Option<i64>) {
    let sha = git(dir, &["rev-parse", "HEAD"]).unwrap_or_default();
    let branch = git(dir, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_default();
    let dirty = git(dir, &["status", "--porcelain", "--untracked-files=no"])
        .map(|s| s.lines().filter(|l| !l.trim().is_empty()).count() as i64);
    (sha, branch, dirty)
}

fn gather_start(worker: &str) -> WorldState {
    let cfg = sv::parse_env(worker);
    let work_dir = sv::session_work_dir(worker);
    let (sha, branch, dirty) = world_at(&work_dir);
    let provider = sv::provider_of(&cfg);
    let model = sv::configured_model_for(&provider, cfg.get_or("CC_MODEL", ""), cfg.get_or("CC_FLAGS", ""));
    let home = sv::home();
    let env_layers: Vec<Value> = sv::scope_env_layers(&home, worker)
        .iter()
        .map(|f| {
            json!({
                "layer": f.strip_prefix(&home).unwrap_or(f).to_string_lossy(),
                "sha256": sha256_file(f),
            })
        })
        .collect();
    WorldState {
        work_dir,
        sha,
        branch,
        dirty,
        conv_id: sv::meta_str(&sv::load_meta(worker), "cc_conversation_id"),
        provider,
        model,
        env_layers: Value::Array(env_layers),
        memory_sha256: sha256_file(&home.join("memory").join(format!("{worker}.md"))),
    }
}

pub(crate) fn gather_end(dir: &str, start_sha: &str) -> EndWorld {
    let sha = git(dir, &["rev-parse", "HEAD"]).unwrap_or_default();
    if sha.is_empty() || start_sha.is_empty() {
        return EndWorld { sha, commits: None, diff_stat: String::new() };
    }
    let range = format!("{start_sha}..{sha}");
    EndWorld {
        commits: git(dir, &["rev-list", "--count", &range]).and_then(|s| s.parse().ok()),
        diff_stat: git(dir, &["diff", "--shortstat", start_sha, &sha]).unwrap_or_default(),
        sha,
    }
}

/// One pass. Returns (starts written, ends written).
pub(crate) fn tick(state: &AppState) -> (usize, usize) {
    let now = chrono::Utc::now().timestamp();
    // FIRST, before any read: the pending query LEFT JOINs this table, so on a
    // fresh database it would fail, read as "nothing pending", and the insert
    // that creates the table would never run. Nothing would ever be recorded.
    if let Err(e) = state.store.write(|c| {
        ensure_table(c)?;
        Ok(crate::db::WriteOutcome { applied: false, events: vec![] })
    }) {
        tracing::warn!(target: "amux::episodes", error = %e, measured = false, n_considered = 0,
            verdict = "episodes_unmeasured", "episode table could not be ensured; no episodes recorded this tick");
        return (0, 0);
    }
    let (starts, ends) = {
        let Ok(conn) = state.store.read() else {
            return (0, 0);
        };
        (
            pending_starts(&conn, now).unwrap_or_default(),
            pending_ends(&conn).unwrap_or_default(),
        )
    };
    let mut wrote = (0usize, 0usize);
    for p in starts {
        let world = gather_start(&p.worker);
        let build = state.build_hash.clone();
        let (pw, ww) = (p.clone(), world.clone());
        let ok = state
            .store
            .write(move |c| {
                let applied = insert_start(c, &pw, &ww, &build, now)?;
                Ok(crate::db::WriteOutcome { applied, events: vec![] })
            })
            .is_ok();
        if ok {
            wrote.0 += 1;
            tracing::info!(target: "amux::episodes", card = %p.card, attempt = p.attempt, worker = %p.worker,
                attempt_id = p.attempt_id, start_sha = %world.sha, conv_id = %world.conv_id,
                start_lag_s = (now - p.started_at).max(0), measured = true, n_considered = 1,
                verdict = "episode_started", "episode start state recorded");
        } else {
            tracing::warn!(target: "amux::episodes", card = %p.card, attempt_id = p.attempt_id,
                measured = false, n_considered = 1, verdict = "episode_start_write_failed",
                "episode start state could not be written; it will be retried while the attempt is young");
        }
    }
    for p in ends {
        let world = gather_end(&p.work_dir, &p.start_sha);
        let (pw, ww) = (p.clone(), world.clone());
        let ok = state
            .store
            .write(move |c| {
                let applied = write_end(c, &pw, &ww, now)?;
                Ok(crate::db::WriteOutcome { applied, events: vec![] })
            })
            .is_ok();
        if ok {
            wrote.1 += 1;
            tracing::info!(target: "amux::episodes", card = %p.card, worker = %p.worker,
                attempt_id = p.attempt_id, outcome = p.outcome.as_deref().unwrap_or(""),
                end_sha = %world.sha, commits = world.commits.unwrap_or(-1), measured = true,
                n_considered = 1, verdict = "episode_closed", "episode end state recorded");
        } else {
            tracing::warn!(target: "amux::episodes", card = %p.card, attempt_id = p.attempt_id,
                measured = false, n_considered = 1, verdict = "episode_end_write_failed",
                "episode end state could not be written; it will be retried next tick");
        }
    }
    if wrote != (0, 0) {
        tracing::info!(target: "amux::episodes", started = wrote.0, closed = wrote.1, measured = true,
            n_considered = wrote.0 + wrote.1, verdict = "episodes_tick", "episode records written");
    }
    wrote
}

pub fn spawn(state: AppState) -> super::PeriodicTask {
    super::spawn_periodic(JOB, tick_secs(), move || {
        let state = state.clone();
        async move {
            let _ = tokio::task::spawn_blocking(move || {
                tick(&state);
            })
            .await;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn() -> (crate::db::Store, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::db::Store::open(&dir.path().join("e.db")).unwrap();
        (store, dir)
    }

    fn seed(c: &Connection, started: i64) -> i64 {
        crate::db::attempts::ensure_table(c).unwrap();
        c.execute(
            "INSERT INTO issues (id, title, desc, status, session, creator, created, updated, type) \
             VALUES ('EP-1', 'Fix the widget', 'Make the widget stop wobbling', 'doing', 'lane', 'test', ?1, ?1, 'code')",
            [started],
        )
        .unwrap();
        c.execute(
            "INSERT INTO task_attempts (card, attempt, worker, generation, started_at) VALUES ('EP-1', 1, 'lane', 1, ?1)",
            [started],
        )
        .unwrap();
        c.last_insert_rowid()
    }

    fn world() -> WorldState {
        WorldState {
            work_dir: "/repo".into(),
            sha: "aaa111".into(),
            branch: "main".into(),
            dirty: Some(2),
            conv_id: "conv-1".into(),
            provider: "claude".into(),
            model: "claude-opus-5-5".into(),
            env_layers: json!([{"layer": "env/g.env", "sha256": "ff"}]),
            memory_sha256: "ee".into(),
        }
    }

    /// The whole life of one episode: found, started once, closed once, and
    /// self-contained afterwards.
    #[test]
    fn an_attempt_gets_one_start_and_one_end_and_survives_its_join_rows() {
        let (store, _d) = conn();
        let t0 = 1_800_000_000i64;
        store
            .write(move |c| {
                let id = seed(c, t0);
                ensure_table(c)?;

                let pend = pending_starts(c, t0 + 10)?;
                assert_eq!(pend.len(), 1);
                assert_eq!(pend[0].attempt_id, id);
                assert!(insert_start(c, &pend[0], &world(), "build-1", t0 + 10)?);
                // A second capture must not overwrite the first read.
                let mut later = world();
                later.sha = "zzz999".into();
                assert!(!insert_start(c, &pend[0], &later, "build-2", t0 + 500)?);
                assert!(pending_starts(c, t0 + 20)?.is_empty(), "a recorded attempt is no longer pending");

                let (sha, lag, after_end, card_start): (String, i64, i64, String) = c.query_row(
                    "SELECT start_sha, start_lag_s, start_after_end, card_start FROM task_episodes WHERE attempt_id=?1",
                    [id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )?;
                assert_eq!(sha, "aaa111");
                assert_eq!(lag, 10);
                assert_eq!(after_end, 0);
                let card: Value = serde_json::from_str(&card_start).unwrap();
                assert_eq!(card["title"], "Fix the widget");
                assert_eq!(card["type"], "code");

                // Still running: nothing to close.
                assert!(pending_ends(c)?.is_empty());

                // The attempt ends; the card moves on and is rewritten.
                c.execute(
                    "UPDATE task_attempts SET ended_at=?1, outcome='done', to_status='done', ended_by='lane' WHERE id=?2",
                    params![t0 + 900, id],
                )?;
                c.execute("UPDATE issues SET status='done', evidence='cargo test -> ok', desc='rewritten later' WHERE id='EP-1'", [])?;
                let ends = pending_ends(c)?;
                assert_eq!(ends.len(), 1);
                assert_eq!(ends[0].start_sha, "aaa111");
                let end = EndWorld { sha: "bbb222".into(), commits: Some(3), diff_stat: "2 files changed".into() };
                assert!(write_end(c, &ends[0], &end, t0 + 905)?);
                assert!(!write_end(c, &ends[0], &end, t0 + 999)?, "an episode closes once");
                assert!(pending_ends(c)?.is_empty());

                // Pruning the join rows must not erase the episode.
                c.execute("DELETE FROM task_attempts", [])?;
                c.execute("DELETE FROM issues", [])?;
                let (outcome, end_sha, commits, card_start, card_end, transcripts): (String, String, i64, String, String, String) =
                    c.query_row(
                        "SELECT outcome, end_sha, commits, card_start, card_end, transcripts FROM task_episodes WHERE attempt_id=?1",
                        [id],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
                    )?;
                assert_eq!((outcome.as_str(), end_sha.as_str(), commits), ("done", "bbb222", 3));
                let start: Value = serde_json::from_str(&card_start).unwrap();
                assert_eq!(start["desc"], "Make the widget stop wobbling", "the START text is what the worker was given");
                let endc: Value = serde_json::from_str(&card_end).unwrap();
                assert_eq!(endc["evidence"], "cargo test -> ok");
                let tr: Value = serde_json::from_str(&transcripts).unwrap();
                assert!(tr.get("measured").is_some(), "transcripts say whether the archive was read: {tr}");
                Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
            })
            .unwrap();
    }

    /// A start read after the attempt closed is flagged, and an attempt too old
    /// to observe is never backfilled.
    #[test]
    fn late_and_stale_starts_are_labelled_not_guessed() {
        let (store, _d) = conn();
        let t0 = 1_800_000_000i64;
        store
            .write(move |c| {
                let id = seed(c, t0);
                ensure_table(c)?;
                c.execute("UPDATE task_attempts SET ended_at=?1, outcome='done' WHERE id=?2", params![t0 + 4, id])?;
                let pend = pending_starts(c, t0 + 9)?;
                assert_eq!(pend.len(), 1);
                assert!(insert_start(c, &pend[0], &world(), "b", t0 + 9)?);
                let after_end: i64 =
                    c.query_row("SELECT start_after_end FROM task_episodes WHERE attempt_id=?1", [id], |r| r.get(0))?;
                assert_eq!(after_end, 1, "a start captured after the end must say so");

                c.execute(
                    "INSERT INTO task_attempts (card, attempt, worker, generation, started_at) VALUES ('EP-1', 2, 'lane', 2, ?1)",
                    [t0],
                )?;
                let far = t0 + START_CAPTURE_MAX_AGE_S + 60;
                assert!(pending_starts(c, far)?.is_empty(), "an attempt older than the window is not backfilled");
                Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
            })
            .unwrap();
    }

    /// The real tick on a FRESH database, where `task_episodes` does not exist
    /// yet. Before the ensure-first fix the pending query failed here, read as
    /// "nothing pending", and nothing was ever recorded.
    #[test]
    fn the_first_tick_on_a_fresh_database_records_the_attempt() {
        let dir = tempfile::tempdir().unwrap();
        let _home = crate::api::settings::test_env::set_home(dir.path());
        let state = AppState {
            store: std::sync::Arc::new(crate::db::Store::open(&dir.path().join("f.db")).unwrap()),
            started: std::time::Instant::now(),
            build_hash: "test-build".into(),
            auth_token: None,
            reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        };
        let now = chrono::Utc::now().timestamp();
        state
            .store
            .write(move |c| {
                seed(c, now - 5);
                Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
            })
            .unwrap();
        assert_eq!(tick(&state), (1, 0), "the first tick must record the start");
        let conn = state.store.read().unwrap();
        let (card, build): (String, String) = conn
            .query_row("SELECT card, amux_build FROM task_episodes", [], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap();
        assert_eq!((card.as_str(), build.as_str()), ("EP-1", "test-build"));
    }

    /// The git half, against a real repository: start sha, then one commit.
    #[test]
    fn git_state_is_read_from_the_worker_directory() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().to_str().unwrap();
        let run = |args: &[&str]| {
            assert!(std::process::Command::new("git").arg("-C").arg(d).args(args)
                .env("GIT_AUTHOR_NAME", "t").env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t").env("GIT_COMMITTER_EMAIL", "t@t")
                .output().unwrap().status.success(), "git {args:?}");
        };
        run(&["init", "-q"]);
        std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        run(&["add", "a.txt"]);
        run(&["commit", "-q", "-m", "first"]);
        let (sha, _branch, dirty) = world_at(d);
        assert_eq!(sha.len(), 40);
        assert_eq!(dirty, Some(0));

        std::fs::write(dir.path().join("a.txt"), "one\ntwo\n").unwrap();
        assert_eq!(world_at(d).2, Some(1), "an uncommitted edit is counted");
        run(&["commit", "-q", "-am", "second"]);
        let end = gather_end(d, &sha);
        assert_ne!(end.sha, sha);
        assert_eq!(end.commits, Some(1));
        assert!(end.diff_stat.contains("1 file changed"), "{}", end.diff_stat);

        // No directory, no claim: empty, never a guess.
        assert_eq!(world_at("/nonexistent/dir").0, "");
        assert_eq!(gather_end("/nonexistent/dir", &sha), EndWorld::default());
    }
}
