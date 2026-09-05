//! SQLite store: WAL mode, single-writer task, read pool, migrations,
//! global revision counter (RR-0019, Invariants 35/36).
//!
//! Concurrency design (plan §SQLite concurrency design):
//! - One dedicated writer thread owns the only write connection. Mutations
//!   arrive over an mpsc channel as closures; the writer applies each inside
//!   a transaction that ALSO bumps the global revision when the mutation
//!   reports itself as a real change. Python's GIL serialized writes by
//!   accident; this serializes them by construction, so `SQLITE_BUSY` cannot
//!   happen under load.
//! - Readers come from an r2d2 pool of read-only connections with a 5s busy
//!   timeout.
//! - The revision lives in `_amux_rev` (single row) and is returned from
//!   every mutation so SSE/delta-sync can publish revisioned StateEvents
//!   (Invariant 35).

pub mod advance;
pub mod artifact_store;
pub mod board_store;
pub mod commands;
pub mod memories;
pub mod migrate;
pub mod queries;
pub mod replay;
pub mod telegram;
pub mod verification_store;
pub mod workflow_store;

use amux_core::revision::{MutationKind, StateEvent, StateRevision};
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::Connection;
use std::path::Path;
use std::sync::mpsc;
use std::sync::Arc;

pub type ReadPool = r2d2::Pool<SqliteConnectionManager>;

/// What a write closure reports back: did it change anything, and what
/// StateEvents should be published if it did. `applied: false` writes do NOT
/// bump the revision (Invariant 37: no-op mutations must be visible as
/// no-ops, not disguised as changes).
pub struct WriteOutcome {
    pub applied: bool,
    pub events: Vec<PendingEvent>,
}

/// A StateEvent minus the revision, which the writer assigns at commit time
/// so event order and revision order can never disagree.
pub struct PendingEvent {
    pub entity_type: amux_core::revision::EntityType,
    pub entity_id: String,
    pub mutation: MutationKind,
    /// RR-0111a: the POST-MUTATION snapshot of the entity row, journaled in
    /// the same transaction as the mutation so state can be replayed from
    /// events alone (plan Invariant 24, EventPayload::Inline). The row is in
    /// the writer's hand when the event is built, so a snapshot costs one
    /// serialization, never a re-read.
    ///
    /// `None` is honest, not lazy: it means this event records THAT the
    /// entity changed, without the state it changed into. Replay
    /// (`db::replay`) reports such entities under `pre_payload_horizon`
    /// instead of pretending an older snapshot is current. Worker and board
    /// (task) mutations populate this; other sites may stay `None` until
    /// their entities need replay.
    pub payload: Option<serde_json::Value>,
}

type WriteFn = Box<dyn FnOnce(&Connection) -> rusqlite::Result<WriteOutcome> + Send>;

struct WriteRequest {
    work: WriteFn,
    reply: mpsc::Sender<rusqlite::Result<WriteReply>>,
}

pub struct WriteReply {
    pub applied: bool,
    pub rev: StateRevision,
    pub events: Vec<StateEvent>,
}

/// Handle to the store: cheap to clone, shared across the router and
/// background jobs.
#[derive(Clone)]
pub struct Store {
    write_tx: mpsc::Sender<WriteRequest>,
    read_pool: ReadPool,
    /// Broadcast of committed StateEvents for SSE fan-out.
    events_tx: tokio::sync::broadcast::Sender<StateEvent>,
}

impl Store {
    /// Open the store: apply migrations, start the writer thread, build the
    /// read pool.
    pub fn open(db_path: &Path) -> anyhow::Result<Store> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Migrations run on a dedicated connection before anything else may
        // touch the DB. Health returns 503 until `open` completes.
        let mut conn = Connection::open(db_path)?;
        configure_connection(&conn)?;
        migrate::apply_all_guarded(&mut conn, db_path)?;

        let (write_tx, write_rx) = mpsc::channel::<WriteRequest>();
        let (events_tx, _) = tokio::sync::broadcast::channel(4096);
        let events_for_writer = events_tx.clone();

        // The writer thread. Plain OS thread, not a tokio task: rusqlite is
        // synchronous and a blocked writer must never stall the async
        // runtime's worker pool.
        std::thread::Builder::new()
            .name("amux-writer".into())
            .spawn(move || writer_loop(conn, write_rx, events_for_writer))
            .expect("spawn writer thread");

        let manager = SqliteConnectionManager::file(db_path).with_init(|c| {
            configure_connection(c)?;
            // Readers never write; enforce it so a bug cannot sneak a write
            // past the single-writer discipline.
            c.pragma_update(None, "query_only", "ON")?;
            Ok(())
        });
        let read_pool = r2d2::Pool::builder()
            .max_size(std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(4))
            .build(manager)?;

        Ok(Store {
            write_tx,
            read_pool,
            events_tx,
        })
    }

    /// Run a mutation on the writer thread and wait for commit. Returns the
    /// revision assigned to this write (unchanged if the write was a no-op).
    pub fn write<F>(&self, f: F) -> anyhow::Result<WriteReply>
    where
        F: FnOnce(&Connection) -> rusqlite::Result<WriteOutcome> + Send + 'static,
    {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.write_tx
            .send(WriteRequest {
                work: Box::new(f),
                reply: reply_tx,
            })
            .map_err(|_| anyhow::anyhow!("writer thread is gone"))?;
        Ok(reply_rx.recv()??)
    }

    /// Async wrapper: parks the wait on the blocking pool so an API handler
    /// can await a write without pinning a runtime worker.
    pub async fn write_async<F>(&self, f: F) -> anyhow::Result<WriteReply>
    where
        F: FnOnce(&Connection) -> rusqlite::Result<WriteOutcome> + Send + 'static,
    {
        let this = self.clone();
        tokio::task::spawn_blocking(move || this.write(f)).await?
    }

    /// Borrow a read-only connection from the pool.
    pub fn read(&self) -> anyhow::Result<r2d2::PooledConnection<SqliteConnectionManager>> {
        Ok(self.read_pool.get()?)
    }

    /// Current global revision.
    pub fn current_rev(&self) -> anyhow::Result<StateRevision> {
        let conn = self.read()?;
        let rev: u64 = conn.query_row("SELECT rev FROM _amux_rev WHERE id = 1", [], |r| r.get(0))?;
        Ok(StateRevision(rev))
    }

    /// Subscribe to committed StateEvents (SSE fan-out).
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<StateEvent> {
        self.events_tx.subscribe()
    }

    /// StateEvents since a revision, for delta sync (RR-0024). Returns
    /// (events, full_sync_required): when the requested window is no longer
    /// in the event journal the client must full-sync rather than trust a
    /// silently incomplete delta (Invariant 40 — an omission must announce
    /// itself).
    pub fn events_since(&self, since: StateRevision, limit: usize) -> anyhow::Result<(Vec<StateEvent>, bool)> {
        let conn = self.read()?;
        let oldest: Option<u64> = conn
            .query_row("SELECT MIN(rev) FROM _amux_state_events", [], |r| r.get(0))
            .unwrap_or(None);
        // Gap check: if the journal's oldest retained event is newer than
        // since+1 and the client is behind that, the delta would be missing
        // events it has no way to detect.
        if let Some(oldest) = oldest {
            if since.0 + 1 < oldest {
                return Ok((vec![], true));
            }
        }
        let mut stmt = conn.prepare(
            "SELECT rev, entity_type, entity_id, mutation, at FROM _amux_state_events
             WHERE rev > ?1 ORDER BY rev ASC LIMIT ?2",
        )?;
        let rows = stmt.query_map(rusqlite::params![since.0, limit as i64], |r| {
            let rev: u64 = r.get(0)?;
            let entity_type: String = r.get(1)?;
            let entity_id: String = r.get(2)?;
            let mutation: String = r.get(3)?;
            let at: String = r.get(4)?;
            Ok((rev, entity_type, entity_id, mutation, at))
        })?;
        let mut events = Vec::new();
        for row in rows {
            let (rev, entity_type, entity_id, mutation, at) = row?;
            events.push(StateEvent {
                rev: StateRevision(rev),
                entity_type: parse_entity_type(&entity_type),
                entity_id,
                mutation: serde_json::from_str(&mutation)
                    .unwrap_or(MutationKind::Updated),
                at: at.parse().unwrap_or_default(),
            });
        }
        Ok((events, false))
    }
}

/// Parse a stored `entity_type` column back into the enum: the bare tag
/// ("worker", "fleet_progress" — the current storage format), with tolerance
/// for the legacy adjacently-tagged object ({"kind":"worker"} /
/// {"kind":"other","data":"x"}) that rows written before the bare-tag fix
/// still carry. Unknown tags land in `Other(tag)` — the open-enum contract.
/// (The previous reader wrapped the raw value in quotes and fed it to serde,
/// which CANNOT parse an adjacently-tagged unit variant from a JSON string —
/// so every event round-tripped as Other(...), for typed variants too.)
fn parse_entity_type(raw: &str) -> amux_core::revision::EntityType {
    use amux_core::revision::EntityType;
    if raw.starts_with('{') {
        if let Ok(t) = serde_json::from_str::<EntityType>(raw) {
            return t;
        }
    }
    serde_json::from_str::<EntityType>(&format!("{{\"kind\":\"{raw}\"}}"))
        .unwrap_or_else(|_| EntityType::Other(raw.to_string()))
}

fn configure_connection(c: &Connection) -> rusqlite::Result<()> {
    c.pragma_update(None, "journal_mode", "WAL")?;
    c.pragma_update(None, "synchronous", "NORMAL")?;
    c.pragma_update(None, "foreign_keys", "ON")?;
    c.busy_timeout(std::time::Duration::from_secs(5))?;
    Ok(())
}

fn writer_loop(
    conn: Connection,
    rx: mpsc::Receiver<WriteRequest>,
    events_tx: tokio::sync::broadcast::Sender<StateEvent>,
) {
    while let Ok(req) = rx.recv() {
        let result = apply_write(&conn, req.work, &events_tx);
        // A dropped reply receiver just means the caller gave up waiting;
        // the write itself has already committed either way.
        let _ = req.reply.send(result);
    }
    // Channel closed = Store dropped = shutdown. Nothing to clean up: WAL
    // checkpoints on connection close.
}

fn apply_write(
    conn: &Connection,
    work: WriteFn,
    events_tx: &tokio::sync::broadcast::Sender<StateEvent>,
) -> rusqlite::Result<WriteReply> {
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let outcome = match work(conn) {
        Ok(o) => o,
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            return Err(e);
        }
    };
    let mut committed_events = Vec::new();
    let rev = if outcome.applied {
        // Bump the global revision once per applied transaction; every event
        // from this transaction shares the revision, which is what makes
        // "give me everything after rev N" exact.
        conn.execute("UPDATE _amux_rev SET rev = rev + 1 WHERE id = 1", [])?;
        let rev: u64 = conn.query_row("SELECT rev FROM _amux_rev WHERE id = 1", [], |r| r.get(0))?;
        let now = chrono::Utc::now();
        for ev in outcome.events {
            // The COLUMN stores the BARE tag ("worker", "task",
            // "fleet_progress"), never serde's adjacently-tagged object.
            // Three consumers filter on `entity_type = '<tag>'` — the
            // redistribute dedupe, /api/metrics/fleet's last-event lookups,
            // and the breaker's window_stats — and all three silently
            // matched NOTHING while this column held {"kind":"task"}
            // (the previous trim_matches('"') stripped quotes from a shape
            // serde never produces for this enum; caught by RR-0111a's
            // replay work + the redistribute dedupe test). Old rows in
            // existing DBs may still carry the object shape, so READERS
            // stay tolerant of both: parse_entity_type below,
            // db::replay::entity_tag.
            let entity_type_str = match &ev.entity_type {
                amux_core::revision::EntityType::Other(s) => s.clone(),
                t => serde_json::to_value(t)
                    .ok()
                    .and_then(|v| v.get("kind").and_then(|k| k.as_str()).map(str::to_string))
                    .unwrap_or_else(|| "other".into()),
            };
            let mutation_json = serde_json::to_string(&ev.mutation).unwrap_or_default();
            // Snapshot rides in the same INSERT as the event it describes —
            // journal row and payload cannot disagree about which transaction
            // produced them (RR-0111a).
            let payload_json = ev.payload.as_ref().map(|p| p.to_string());
            conn.execute(
                "INSERT INTO _amux_state_events (rev, entity_type, entity_id, mutation, at, payload)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![rev, entity_type_str, ev.entity_id, mutation_json, now.to_rfc3339(), payload_json],
            )?;
            committed_events.push(StateEvent {
                rev: StateRevision(rev),
                entity_type: ev.entity_type,
                entity_id: ev.entity_id,
                mutation: ev.mutation,
                at: now,
            });
        }
        StateRevision(rev)
    } else {
        let rev: u64 = conn.query_row("SELECT rev FROM _amux_rev WHERE id = 1", [], |r| r.get(0))?;
        StateRevision(rev)
    };
    conn.execute_batch("COMMIT")?;
    // Publish only after commit: a subscriber must never see an event whose
    // transaction later rolled back.
    for ev in &committed_events {
        let _ = events_tx.send(ev.clone());
    }
    Ok(WriteReply {
        applied: outcome.applied,
        rev,
        events: committed_events,
    })
}

/// Shared handle used by API state.
pub type SharedStore = Arc<Store>;
